# Making the macOS capture → encode path as fast as it can be, on every macOS from 10.15 to 26

## 1. Question

How do we make the macOS capture → encode path as fast as it can be on **every** macOS from 10.15
Catalina to macOS 26 Tahoe, with every API reached at runtime (no link-time reference to anything
newer than 10.15)? Specifically: which ScreenCaptureKit, CGDisplayStream, VideoToolbox, Metal and
scheduling knobs actually move latency; what each one costs; which OS each needs; how to probe for
it rather than version-check it; and why the same Mac feels usable only when every other app is
quit.

The user's dev box is macOS 10.15.8 on Intel, where **CGDisplayStream is the only capture path** and
VideoToolbox's low-latency rate control does not exist. So the answer has to be good on the oldest
supported OS, not only on Apple silicon.

A companion report, `docs/research/airplay-why-fast.md`, already covers the *transport* half of the
same user complaint (AWDL, the WebRTC jitter buffer, host-AP channels). This report is the
host-side capture and encode half; it does not repeat that ground, and where the two touch — the
`kCGDisplayStreamMinimumFrameTime` hold-off — this report supersedes it with the primary source.

---

## 2. Findings

### 2.1 CGDisplayStream — what the header actually says

Everything in this section is quoted from Apple's `CGDisplayStream.h` in the **10.15 SDK**
([mirror](https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.15.sdk/System/Library/Frameworks/CoreGraphics.framework/Versions/A/Headers/CGDisplayStream.h)),
which is the authority for the dev box.

| Key | Type / default | What Apple says | What ScreenExtend does |
|---|---|---|---|
| `kCGDisplayStreamMinimumFrameTime` | CFNumber seconds, **defaults to zero** | "Request that the delta between frame updates be at least as much specified by this value." | Sets `(1/fps)·0.95` — `cgds.rs:70` |
| `kCGDisplayStreamQueueDepth` | frames, **defaults to 3** | "Controls how many frames deep the frame queue will be." | Sets **2** — `cgds.rs:71` |
| `kCGDisplayStreamShowCursor` | CFBoolean, defaults **false** | cursor embedded in the buffer by WindowServer | Sets true — `cgds.rs:72` |
| `kCGDisplayStreamPreserveAspectRatio` | CFBoolean, defaults **true** | letterboxes when source and destination aspect differ | Sets false — `cgds.rs:73` |
| `kCGDisplayStreamColorSpace` | CGColorSpaceRef, **defaults to the display's colour space** | "Set the desired CGColorSpace of the output frames." | **Not set** |
| `kCGDisplayStreamYCbCrMatrix` | CFString | "When outputting frames in 420v or 420f format… control which YCbCr matrix is used" | 709 — `cgds.rs:81`, `:88` |
| `kCGDisplayStreamSourceRect` | CGRect **in logical points** | "request a subregion of the display" | **Not set** |
| `kCGDisplayStreamDestinationRect` | CGRect **in output pixels** | "where within the destination buffer the display updates should be placed" | **Not set** |

Four consequences follow directly.

**(a) The `minimumFrameTime` hold-off is self-inflicted.** The default is *zero*. ScreenExtend is not
relaxing a throttle, it is *adding* one, and a hold-off delays every frame that lands mid-interval:
uniform 0…16.7 ms at 60 fps, mean ~8.3 ms. Omitting the key is both the correct experiment and the
safe one. OBS's macOS display capture does not set it at all — its whole property dictionary is
`{ SourceRect, QueueDepth: 5, ShowCursor }`
([obs `mac-display-capture.m`](https://github.com/alibaba/tblive/blob/master/plugins/mac-capture/mac-display-capture.m#L204-L209)).

**(b) `queueDepth = 2` is below Apple's own default of 3.** OBS uses 5. The SCK sibling of the key
has a documented default that Apple *raised* from 3 to 8 over the framework's life (§2.2).

**(c) The IOSurface use-count contract is not being honoured — and this is the sharpest finding in
this report.** The handler documentation is explicit:

> "If you intend to hold on to the IOSurface beyond the lifetime of the handler call, you must
> `CFRetain()` the IOSurface until you are done with it **and you must call
> `IOSurfaceIncrementUseCount()`** to let the CGDisplayStream know that the frame is not ready for
> re-use. Once you are finished using the IOSurfaceRef you must then call
> `IOSurfaceDecrementUseCount()`."

ScreenExtend does the retain (`cgds.rs:134`) and never the use count — the symbol appears nowhere in
the crate. `CVPixelBufferCreateWithIOSurface` does not cover it either: its own header says only
*"The CVPixelBuffer will **retain** the IOSurface"*
([`CVPixelBufferIOSurface.h`](https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.15.sdk/System/Library/Frameworks/CoreVideo.framework/Versions/A/Headers/CVPixelBufferIOSurface.h)),
and Apple documents the two counters as independent — *"the use count refers to the IOSurface's
usage by an API and differs from its retain count"*. OBS pairs them correctly on every frame.

This **inverts** the existing `blocker-cgds-queue-depth-starvation.md` hypothesis. With the use
count at zero, CoreGraphics believes every surface is free, so the stream does not starve — it
recycles buffers we are still encoding. The symptom is tearing and stale bands that get worse under
load, not a low frame rate. Fixing the use count *without* also raising `queueDepth` converts a
corruption bug into a genuine starvation bug, so the two must ship together.

**(d) The API's own performance counter is never read.** `CGDisplayStreamUpdateGetDropCount` is
documented as *"primarily useful for performance measurement to determine if the client is keeping
up with all WindowServer updates"*. It arrives on the `CGDisplayStreamUpdateRef` that
`handle_frame` already receives and inspects for dirty rects (`cgds.rs:110-126`). OBS logs it on
every delivery. This is the cheapest measurement available in this territory and it settles (a),
(b) and (c) empirically.

Two more header facts worth recording:

- `displayTime` is *"the `mach_absolute_time()` of when the corresponding frame was to be displayed
  by the WindowServer"* — the true capture instant, bound and discarded at `cgds.rs:172`.
- The dirty-rect types are `RefreshedRects`, `MovedRects`, `DirtyRects` (*"the union of both
  refreshed and moved rects"*) and `ReducedDirtyRects` (*"a possibly simplified (but overstated)
  array"*). `CGDisplayStreamUpdateCreateMergedUpdate` exists specifically so a client that skips
  frames can coalesce the deltas — relevant if the dirty-rect skip at `cgds.rs:116-126` is kept.
- Supported output pixel formats are exactly `'BGRA'`, `'l10r'`, `'420v'`, `'420f'`.

**Status on modern macOS.** CGDisplayStream was deprecated in the 13.3 SDK
([forum 726289](https://developer.apple.com/forums/thread/726289)) and marked **obsoleted** in the
macOS 15 SDK — `CGDisplayStreamStart`, `CGDisplayStreamStop`, `CGDisplayStreamUpdateGetRects`,
`CGDisplayStreamUpdateCreateMergedUpdate`, `CGDisplayStreamCreateWithDispatchQueue` and
`kCGDisplayStreamShowCursor` among them
([FreeRDP #10558](https://github.com/FreeRDP/FreeRDP/issues/10558)). Separately, Sequoia's recurring
screen-capture prompt singles out apps still using it, warning the user the app *"might be able to
collect detailed information about the user"*
([forum 761443](https://developer.apple.com/forums/thread/761443),
[Purple Computing](https://purplecomputing.com/articles/macos-sequoia-new-weekly-permission-prompt/);
cadence softened from weekly to monthly before release,
[TechRadar](https://www.techradar.com/computing/mac-os/on-macos-sequoia-youll-only-have-to-deal-with-screen-recording-permission-pop-ups-once-a-month-rather-than-once-a-week)).
Reports agree the API still *functions* at runtime. ScreenExtend imports its symbols at **link
time** (`cgds.rs:16-21` via `objc2-core-graphics`), which is a latent load risk on any future OS
that drops the exports, and — because `mod.rs:107-114` probes TCC with CGDisplayStream even when SCK
will be used — a modern-macOS host can trigger the privacy alert without ever streaming through the
legacy path.

### 2.2 ScreenCaptureKit — what the header actually says

Quoted from `SCStream.h` in the **macOS 26.5 SDK** and the **12.3 SDK**
(via the [alexey-lysiuk/macos-sdk](https://github.com/alexey-lysiuk/macos-sdk) mirror), which is how
the version-to-version drift below was established.

**`minimumFrameInterval`** (12.3):

> "specifies the desired minimum time in seconds between frame updates, **allowing you to throttle
> the rate at which updates are received**. The default value is 1/60, meaning that updates are
> coming in at or up to 60fps. **Set this to `kCMTimeZero` to capture at display's native refresh
> rate.**"

(The "default 1/60" sentence and the `kCMTimeZero` guidance were added after 12.3; the 12.3 header
carries only the first clause.) It is a **throttle**, i.e. a hold-off, with the same
mid-interval-delay characteristic as CGDisplayStream's key. Two independent projects measured what
happens when you set it to exactly `1/fps`:

- **OpenDisplay** — the closest architectural twin to ScreenExtend (open-source Sidecar/Duet
  alternative: `CGVirtualDisplay` → SCK → VideoToolbox H.264 → network, iPad/iPhone/Mac receiver):
  > "Ask for 120 even though the virtual display is 60Hz: requesting exactly 1/60 makes SCK's rate
  > limiter skip frames that arrive a hair early (beat frequency) — **measured ~51fps instead of
  > 60**." ([`MacSender.swift`](https://github.com/peetzweg/opendisplay/blob/main/Mac/MacSender.swift))
- **OBS Studio** PR [#11896](https://github.com/obsproject/obs-studio/pull/11896):
  > "setting this property to the exact inverse of our frame rate seems to provide a capture that
  > drops many frames… The chosen buffer is **10% of our target interval**. This value was
  > determined through subjective testing."
- **libwebrtc** sets `kCMTimeZero` unless an explicit max frame rate was requested
  ([`screen_capturer_sck.mm`](https://github.com/webrtc-mirror/webrtc/blob/main/modules/desktop_capture/mac/screen_capturer_sck.mm)).

ScreenExtend sets exactly `1/fps` (`sck.rs:556-571`) — the configuration all three avoid.

**`queueDepth`**, identical wording in both SDKs except the number:

> "specifies the number of frames to keep in the queue. If not set the default value is **3 frames**
> [12.3] / **8 frames** [26.5]. Specifying more frames uses more memory, but **may allow you to
> process frame data without stalling the display stream** and should not exceed 8 frames."

Apple raised its own default from 3 to 8, and the documented purpose is decoupling, not latency.
ScreenExtend sets 3 with the comment *"3 = lowest latency"* (`sck.rs:41-42`, `:573-574`), which no
source supports. OpenDisplay uses the maximum and says why:

> "One buffer is held permanently (keyframe replay) and one sits in the encoder for ~13ms — headroom
> prevents SCK starvation drops. `config.queueDepth = 8`"

That is ScreenExtend's holding pattern exactly: the `FrameSink` slot (`frame.rs:63-66`), the
200 ms keepalive pin `last_frame` (`pipeline.rs:168-170`, `:202`) and the frame inside VideoToolbox
— three held out of three available.

**`SCFrameStatus`** — six cases, in declaration order so Complete = 0 … **Started = 4**, Stopped = 5:

> Complete "new frame was generated" · Idle "…the display did not change" · Blank "…the display has
> gone blank" · Suspended "…updates haves been suspended" · **Started "new frame that is indicated as
> the first frame sent after the stream has started"** · Stopped "the stream was stopped".

`Started` carries pixels. libwebrtc accepts `Complete || Started`; ScreenExtend accepts only
`Complete` (`sck.rs:87-89`, `:655-660`), so the first frame of every session is thrown away — and on
a *static* virtual display (a freshly created one with no windows) the next `Complete` may not
arrive for an arbitrarily long time.

**Everything else, with versions.** `width`/`height` (default 1920×1080), `pixelFormat`
(`BGRA`, `l10r`, `420v`, `420f`, plus `xf44` and `RGhA` on 15+), `scalesToFit`, `showsCursor`
(*"By default the cursor is visible"*), `backgroundColor`, `sourceRect` (**points**),
`destinationRect` (**pixels**), `queueDepth`, `colorMatrix` (*"Should only be used if your pixel
format is 420v or 420f"*), `colorSpaceName` (*"If not set the output buffer uses the same color
space as the display"*) are all **12.3**. Then: `capturesAudio`, `sampleRate`, `channelCount`,
`excludesCurrentProcessAudio`, `SCStream.synchronizationClock` at **13.0**;
`SCStreamFrameInfoScreenRect` at **13.1**; `preservesAspectRatio`, `streamName`,
`ignoreShadowsDisplay`, `ignoreShadowsSingleWindow`, `captureResolution`, `capturesShadowsOnly`,
`shouldBeOpaque`, `ignoreGlobalClipDisplay`, `ignoreGlobalClipSingleWindow`,
`presenterOverlayPrivacyAlertSetting`, `SCContentFilter.style/pointPixelScale/contentRect`,
`SCStreamFrameInfoBoundingRect` at **14.0**; `includeChildWindows`, `SCContentFilter.includeMenuBar`,
`SCStreamFrameInfoPresenterOverlayContentRect` at **14.2**; `captureMicrophone`,
`microphoneCaptureDeviceID`, `showMouseClicks`, `captureDynamicRange`, `SCStreamConfigurationPreset`,
`SCRecordingOutput`, `SCContentSharingPicker` at **15.0**; `SCContentFilter.includedDisplays/
includedApplications/includedWindows` at **15.2**; and at **26.0** only the HDR10 recording preset
`SCStreamConfigurationPresetCaptureHDRRecordingPreservedSDRHDR10`.

**None of the 14.0+ additions reduce latency.** `capturesShadowsOnly`, `shouldBeOpaque`,
`ignoreShadows*`, `ignoreGlobalClip*`, `presenterOverlayPrivacyAlertSetting`, `showMouseClicks`,
`captureDynamicRange`, `SCRecordingOutput` and `SCContentSharingPicker` are compositing, privacy,
picker and recording features. `captureResolution` (`Automatic`/`Best`/`Nominal`, 14.0) is the only
one with a plausible performance angle — libwebrtc sets `Nominal` — and it selects point-vs-pixel
resolution, not latency. The latency-relevant SCK surface is entirely present at 12.3, and
ScreenExtend gets two of its three settings wrong there.

**Frame timing.** `SCStreamFrameInfoDisplayTime` is *"the **mach absolute time** when the event
occurred. For a frame event, this is when the frame was displayed by the window server"* — the exact
analogue of CGDisplayStream's `displayTime`, and the honest capture instant. `publish_sample`
(`sck.rs:678-723`) reads neither it nor `CMSampleBufferGetPresentationTimeStamp`, stamping
`Instant::now()` at callback entry instead. On 13.0+ `SCStream.synchronizationClock` gives the
`CMClockRef` those timestamps ride, which is how you convert cleanly into the shared host epoch.

**The 12.3 tier is not trustworthy.** libwebrtc gates its SCK capturer at **macOS 14.0**:
> "The ScreenCaptureKit API was available in macOS 12.3, but **full-screen capture was reported to
> be broken before macOS 13** — see http://crbug.com/40234870. Also, the `SCContentFilter` fields
> `contentRect` and `pointPixelScale` were introduced in macOS 14."

Full-screen capture is precisely what ScreenExtend does (`initWithDisplay:excludingWindows:` with an
empty array), and `screencapturekit_available()` switches SCK on at 12.3 by `sw_vers` string compare
(`mod.rs:61-82`).

### 2.3 VideoToolbox — per-key availability, and which keys are reachable from 10.15

Versions below are from `VTCompressionProperties.h`
([iOS 17.4 SDK mirror](https://github.com/xybp888/iOS-SDKs/blob/master/iPhoneOS17.4.sdk/System/Library/Frameworks/VideoToolbox.framework/Headers/VTCompressionProperties.h);
Apple's headers carry `macos()` annotations alongside `ios()`), cross-checked against Apple's
[Compression Properties index](https://developer.apple.com/documentation/videotoolbox/compression-properties).

**Reachable on the 10.15 floor:** `RealTime` (10.9), `AllowFrameReordering` (10.8),
`MaxKeyFrameInterval` / `MaxKeyFrameIntervalDuration` (10.8), `ExpectedFrameRate` (10.8),
`AverageBitRate` (10.8), `DataRateLimits` (10.8), `MaxFrameDelayCount` (10.8),
**`NumberOfPendingFrames` (10.8, read-only)**, `MaxH264SliceBytes` (10.8), `H264EntropyMode` (10.9),
`ProfileLevel` + the non-Constrained AutoLevel constants (10.8/10.9),
`MaximizePowerEfficiency` (10.14), `ColorPrimaries`/`TransferFunction`/`YCbCrMatrix` (10.8),
`UsingHardwareAcceleratedVideoEncoder` (10.9), `EncoderID` (10.13),
`UsingGPURegistryID` (10.15), `kVTEncodeFrameOptionKey_ForceKeyFrame` (10.8), and the
`Enable`/`RequireHardwareAcceleratedVideoEncoder` specification keys (10.9). Note the header states
*"In MacOS 10.15 and later, hardware encode is enabled in VTCompressionSessions by default"*, so the
`Enable` hint is redundant on the floor.

**Above the floor** — every one must be `dlsym`'d, which `encoder.rs:566-580` already does correctly:

| Key | macOS | Effect |
|---|---|---|
| `PrioritizeEncodingSpeedOverQuality` | **11.0** | H.264/HEVC hardware encoders "prioritize quality over speed by default"; this flips it |
| `kVTVideoEncoderSpecification_EnableLowLatencyRateControl` | **11.3** (not 11.0) | see below |
| `BaseLayerFrameRateFraction` | 11.3 | temporal scalability |
| `MaxAllowedFrameQP` | **12.0** | QP ceiling; encoder drops frames when the cap binds |
| `EnableLTR` + `ForceLTRRefresh` + `AcknowledgedLTRTokens` + `RequireLTRAcknowledgementToken` | **12.0** | loss recovery without an IDR |
| `SupportsBaseFrameQP`, `BaseFrameQP` | 12.0 | per-frame QP; disables standard rate control |
| `kVTProfileLevel_H264_Constrained{Baseline,High}_AutoLevel` | **12.0** | — |
| `BaseLayerBitRateFraction` | 12.0 | default 0.6, WWDC recommends 0.6–0.8 |
| `MinAllowedFrameQP` | **13.0** | note the asymmetry with Max (12.0) |
| `ConstantBitRate` | **13.0** | see below |
| `ReferenceBufferCount` | 13.0 | **buggy for H.264, see below** |
| `MaximumRealTimeFrameRate`, `SuggestedLookAheadFrameCount`, `SpatialAdaptiveQPLevel` | 15.0 | |
| `SupportedPresetDictionaries` incl. `kVTCompressionPreset_VideoConferencing` | **26.0** | the only "low-latency bundle" Apple ships, and it is 26-only |
| `VariableBitRate`, `VBVMaxBitRate`, `VBVBufferDuration` | 26.0 | explicit HRD control |

**`EnableLowLatencyRateControl` (11.3).** The header enumerates what it enforces: *"Infinite GOP
(all P frames following the beginning IDR). No frame reordering (B frame) or looking ahead. **Only
High profiles.** Levels are left for the encoder to automatically set. Temporal Layer structure."*
[WWDC21 session 10158](https://developer.apple.com/videos/play/wwdc2021/10158/) claims it reduces
end-to-end latency **by up to 100 ms for 720p30** and adds a rate controller that reacts faster to
network change. Caveats, all from production code:

- It **requires a hardware encoder**; `VTCompressionSessionCreate` returns **-12902** with *"Low
  latency RC mode requires hardware encoder"* even when `RequireHardware…` is also set
  ([forum 684262](https://developer.apple.com/forums/thread/684262), unresolved). Chromium: *"trying
  to use `EnableLowLatencyRateControl` with the SW encoder leads to an initialization error."*
  OpenDisplay hit the same on AMD-GPU Macs and ships a retry-without-the-spec fallback — which is
  what ScreenExtend's ladder already does.
- It selects distinct implementations (`…videoencoder.h264.rtvc`), for which Chromium reports
  *"neither `UsingHardwareAcceleratedVideoEncoder` nor `kVTVideoEncoderList_IsHardwareAccelerated`
  is set"* — so on this path our one hardware signal is meaningless.
- **`DataRateLimits` is incompatible with it.** Chromium, verbatim: *"It is NOT compatible with
  `EnableLowLatencyRateControl` even though this fact is not documented. Even in non low latency mode
  `DataRateLimits` tends to make the encoder **undershoot** set bitrate."*
- The "Only High profiles" clause conflicts with WWDC21's presentation of Constrained Baseline as a
  low-latency feature. **Unresolved.** It matters for browser clients that advertise only `42e01f`.
- **It is not automatically a win.** OpenDisplay measured *"on Apple silicon at a paced 60fps:
  **5.3 ms mean submit→emit without the spec vs 6.1 ms with it**, 1 frame held either way.
  (Overfeeding it at ~320fps does queue ~8 frames, hence the cap.)"* The win in WWDC's number comes
  from removing reordering and look-ahead, both of which `AllowFrameReordering = false` already
  removes.

**`MaxFrameDelayCount` is back-pressure, not a hint.** Header: *"If the maximum frame delay count is
M, then before the call to encode frame N returns, frame N-M must have been emitted."* So `M = 1`
**blocks the calling thread** until the previous frame is out, and `M = 0` is fully synchronous.
ScreenExtend sets `1` on the `THREAD_TIME_CONSTRAINT` real-time encode thread
(`encoder.rs:212-220`, thread configured at `pipeline.rs:133-136`). Practice is split: libwebrtc
tried 1, *"this call just fails"*, and left it commented out; Chromium uses 3 and notes macOS 15.0
rejects it; Firefox uses 0 for realtime; FFmpeg never sets it; OpenDisplay sets 0.

**`DataRateLimits` vs `AverageBitRate` vs `ConstantBitRate`.** Apple's
[QA1958](https://developer.apple.com/library/archive/qa/qa1958/_index.html): `DataRateLimits` is
*"a **hard upper limit** on the data rate for a sliding window"*, `AverageBitRate` is *"just what it
says, an average. There is no hard limit, just a target."* `ConstantBitRate` (13.0) is
*"not compatible with `DataRateLimits` and `AverageBitRate`… The encoder will **pad the frame** if
they are smaller than they need to be… **not recommended for general streaming**"* — padding is
actively wrong for screen content, which should go to near-zero bits on a static desktop. Both OBS
and GStreamer additionally gate CBR on **macOS 13 *and* Apple silicon** (OBS logs *"CBR support for
VideoToolbox encoder requires Apple Silicon. Will use ABR instead."*; GStreamer's `vtenc.c` wraps it
in `#ifdef __aarch64__`).

**`RealTime` is not free either.** OBS ships `RealTime = kCFBooleanFalse` because
[obs-studio#5840](https://github.com/obsproject/obs-studio/issues/5840) found `RealTime = true`
*combined with bitrate limiting* caused heavy frame drops on M1-era Macs. libwebrtc and Chromium set
it true; GStreamer defaults false. ScreenExtend sets `RealTime = true` **and** a hard
`DataRateLimits` cap — precisely the pairing OBS removed.

**Which encoder implementation you get decides more than any property.** Apple Developer Forums
[748918](https://developer.apple.com/forums/thread/748918) reports that under an identical
configuration (`RealTime`, no reordering, High AutoLevel), `…videoencoder.h264.gva` achieved true
one-in/one-out while `…videoencoder.ave.avc` showed **a 4-frame delay, sometimes 2, sometimes 16**.
At 60 fps that is 67–267 ms of pure encoder buffering. `kVTCompressionPropertyKey_EncoderID` (10.13)
reads back which one you got; ScreenExtend never reads it. `NumberOfPendingFrames` (10.8, read-only)
reads the live depth; ScreenExtend never reads that either. GStreamer polls it per frame and had to
stop logging every change because it *"changes way too often"*.

**Hardware-encode minimum resolutions on Intel.** Apple documents none; Chromium carries an
empirical table with the test machines named
([`vt_video_encode_accelerator_mac.mm`](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/media/gpu/mac/vt_video_encode_accelerator_mac.mm)):
Intel H.264 needs **width ≥ 640 or height ≥ 480** (`{640,2}`, `{18,480}`); HEVC needs ≥ 146×50;
Apple silicon needs only 16×16. libwebrtc says the same less precisely: *"below 360p the compression
session will be created with hw accl disabled."* ScreenExtend sizes the encoder from client-chosen
dimensions times a user-set `videoScale`, so a small client at 50 % scale can fall under the Intel
threshold and land silently on the CPU encoder.

**Slices: the answer is no.** `MaxH264SliceBytes` (10.8) caps slice size in bytes "where possible".
There is **no** `NumberOfSlices` key — Apple's complete property index lists `MaxH264SliceBytes` as
the only slice symbol — and there is **no sub-frame output**: the callback delivers whole
`CMSampleBuffer`s and there is no API to emit a slice before the frame completes. No project was
found doing slice-level low-latency output through VideoToolbox. libwebrtc left `MaxH264SliceBytes`
as a never-implemented TODO.

**Full range cannot be signalled by a property.** There is no
`kVTCompressionPropertyKey_FullRangeVideo`; range is carried by the input pixel buffer format
(`420v` vs `420f`), and `YCbCrMatrix` does not write `video_full_range_flag`
([forum 667629](https://developer.apple.com/forums/thread/667629), unresolved). ScreenExtend
captures `420f` (`mod.rs:58`) and tags 709 primaries/transfer/matrix without any range signal.

**`ReferenceBufferCount = 1` is an Apple-silicon H.264 trap**, found independently twice: it makes
VideoToolbox emit **all-IDR** output — *"causing massive bandwidth inflation (~3x) and frame drops.
HEVC and AV1 are unaffected"* ([Sunshine #5013](https://github.com/LizardByte/Sunshine/issues/5013));
Chromium skips it for H.264 on macOS 26. ScreenExtend does not set it; recorded so it is not
"discovered" later.

**HEVC vs H.264 latency:** no controlled millisecond comparison exists on identical Apple hardware.
What is documented is that HEVC's low-latency path is less well-trodden — FFmpeg enables HEVC
low-latency only on ARM64, WebKit needed a create-then-retry fallback because HEVC low-latency can
fail outright, Chromium restricts HEVC SVC to macOS 14+/ARM64 and skips `MaxKeyFrameIntervalDuration`
for HEVC on x64. **HEVC should not be a default on Intel.**

### 2.4 Metal / zero copy

`macos_utils/streamer/gpu.rs` is a 15-line stub: the `MTLDevice` and `CVMetalTextureCache` fields are
commented out and `Gpu::new()` returns an `Arc` of nothing (`gpu.rs:5-14`), despite `CLAUDE.md:354`
describing it as "Metal device + `CVMetalTextureCache`". Filed as
`minor-vt-gpu-struct-is-an-empty-stub-claude-md-says-otherwise.md`.

**That stub is the right answer, and the module should be deleted rather than filled in.** The
current path is already zero-copy end to end: WindowServer composites, scales and converts to
`420f` on the GPU and vends an `IOSurface`; the callback wraps it in a `CVPixelBuffer` with no pixel
copy (`cgds.rs:136`, `sck.rs:706`); VideoToolbox consumes an IOSurface-backed `CVPixelBuffer`
directly. Inserting a Metal scale/convert stage would mean asking for `BGRA`, doing on our own GPU
queue what the compositor already did for free, and adding a second surface allocation per frame.
The only reason to build it would be if measurement showed WindowServer's scaler to be the
bottleneck (`speculative-cgds-windowserver-does-the-downscale-with-no-source-rect.md`), and the
cheaper first move there is `sourceRect`/`destinationRect`, which both APIs expose and neither
backend sets.

The real per-frame allocations that do exist are small and already filed
(`minor-vt-per-frame-allocations-in-hot-path.md`): a `CVPixelBuffer` wrapper object, an
`Arc<Frame>`, and a `Vec<u8>` per encoded access unit.

### 2.5 Scheduling, GPU selection, and why "you must quit every other app"

This is the part of the brief the sections above do not answer, and it turns out to have a
first-party answer rather than a folklore one.

**The documented cause is encoder contention, not CPU scheduling.** From
`VTCompressionProperties.h` in the **10.15 SDK**, the discussion for
`kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder`
([10.15 SDK mirror](https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.15.sdk/System/Library/Frameworks/VideoToolbox.framework/Versions/A/Headers/VTCompressionProperties.h)):

> "Hardware acceleration may be unavailable for a number of reasons. A few common cases are:
> — the machine does not have hardware acceleration capabilities — the requested encoding format
> or encoding configuration is not supported — **the hardware encoding resources on the machine
> are busy**"

The hardware H.264 encoder is a **machine-wide scarce resource**, and Apple ships an API whose
entire purpose is to say so: `kVTVideoEncoderList_InstanceLimit` (**macOS 10.14**,
`VTVideoEncoderList.h`) —

> "optional. CFBoolean. If present and set to kCFBooleanTrue, there is a global instance limit cap
> on this encoder. **Indicates that an encoder is a scarce resource which may potentially be
> unavailable.**"

Two consequences for ScreenExtend specifically. First, any other process holding the encoder — a
video call, QuickTime, OBS, Safari playing a hardware-encoded WebRTC call — can push
`VTCompressionSessionCreate` off the hardware rung. ScreenExtend's ladder (`encoder.rs:73-93`)
then lands on rung 3, `(low_latency=false, require_hw=false)`, where on 10.15+ hardware is merely
*enabled by default* and software is a perfectly legal outcome. Second, ScreenExtend creates **one
`VTCompressionSession` per connected client**, so past some client count it contends with itself.
Neither case is detected, logged, or surfaced; the user's only observable is that the stream is bad
until they quit things, which is precisely the reported symptom.

**The capability probe for all of this already exists, on the floor, and is unused.**
`VTCopySupportedPropertyDictionaryForEncoder(width, height, codecType, encoderSpecification,
&encoderIDOut, &supportedPropertiesOut)` is **macOS 10.13** (`VTVideoEncoderList.h`). It answers,
*without creating a session*: which encoder ID would be selected for these exact dimensions and
this spec, and which properties that encoder supports. That is a direct, first-party replacement
for (a) Chromium's empirical Intel `640×480` threshold table (§2.3 — ask the machine instead of
consulting a table), (b) every `macos_at_least(x, y)` gate in `encoder.rs`, and (c) the
`dlsym`-then-set-and-hope pattern in `set_optional_*`. It is the single best fit in this report for
the project's "probe the capability, never the version number" doctrine. `VTCopyVideoEncoderList`
(10.8) plus `kVTVideoEncoderList_IsHardwareAccelerated` / `_GPURegistryID` / `_InstanceLimit`
(all 10.14) give the same picture for the machine as a whole.

**GPU selection is a second, independent contention story on Intel.** Three keys, all reachable
from the floor:

| Key | macOS | Header text |
|---|---|---|
| `kVTVideoEncoderSpecification_RequiredEncoderGPURegistryID` | **10.14** | "the VideoToolbox will **only** use a hardware encoder running on the GPU specified"; "implies `EnableHardwareAcceleratedVideoEncoder`" |
| `kVTVideoEncoderSpecification_PreferredEncoderGPURegistryID` | **10.14** | "will **try** to use a hardware encoder running on the GPU specified… will fall back to alternate encoders" |
| `kVTCompressionPropertyKey_UsingGPURegistryID` | **10.15** | "returns CFNumber indicating the gpu registryID of the encoder that was selected. **NULL indicates a built-in encoder or software encode was used.**" |

The registry ID "can be obtained from a `MTLDevice` using `[MTLDevice registryID]`", and the GPU
currently driving a given display is `CGDirectDisplayCopyCurrentMetalDevice(displayID)`
(CoreGraphics, 10.11 — below the floor, and reachable by `msg_send!` without linking Metal). On a
dual-GPU Intel MacBook Pro the virtual display and the encoder can end up on different GPUs, which
makes every frame's IOSurface migrate across the PCIe bridge. Note the trap in the third row:
**`UsingGPURegistryID` returns NULL for both an integrated-GPU hardware encoder and a software
encoder**, so it cannot be used on its own to detect the software fallback — that is what
`UsingHardwareAcceleratedVideoEncoder` and `EncoderID` are for (R2, R11).

**And ScreenExtend forces the discrete GPU on, machine-wide, for as long as it runs.** Apple's
[QA1734](https://developer.apple.com/library/archive/qa/qa1734/_index.html):

> "By default, once your application creates an OpenGL context (by either calling OpenGL directly
> or an API that relies on OpenGL such as Core Animation, Core Image, etc), the MacBook Pro
> automatically switches to the higher-end discrete GPU for performance concerns and **won't switch
> back until the application quits**."

The opt-out is one Info.plist boolean, `NSSupportsAutomaticGraphicsSwitching`.
`src-tauri/Info.plist` contains only `NSAudioCaptureUsageDescription` and the ATS dictionary — the
key is absent, and a Tauri app is a WKWebView, which creates exactly the kind of context QA1734
describes. On a 2016–2019 MacBook Pro this means ScreenExtend pins the dGPU on for its whole
lifetime, on a machine that is simultaneously being asked to encode 60 fps of 1080p. Sustained
dGPU power on a chassis like that is a thermal-throttle generator, and thermal throttling is
indistinguishable, from the user's chair, from "it only works if I quit everything".

**The thread-scheduling stack is misconfigured, and the primary sources are unambiguous.**

Apple's `pthread/qos.h`
([libpthread](https://github.com/apple-oss-distributions/libpthread/blob/main/include/pthread/qos.h)):

> "Subsequent calls to interfaces such as `pthread_setschedparam()` that are incompatible or in
> conflict with the QOS class system **will unset the QOS class requested with this interface** and
> `pthread_get_qos_class_np()` will return `QOS_CLASS_UNSPECIFIED` thereafter. **A thread so
> modified is permanently opted-out of the QOS class system** and calls to this function to request
> a QOS class for such a thread will fail and return EPERM."

`run_encode_loop` (`pipeline.rs:133-137`) does exactly the conflicting sequence: QoS class first,
then three `thread_policy_set` calls. Every return code is discarded, so which of the two outcomes
occurs on 10.15 is currently unknowable from the logs.

Chromium's `platform_thread_apple.mm`
([source](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/base/threading/platform_thread_apple.mm))
is the best-tested real-time-thread implementation in the open, and it differs from ScreenExtend in
two measurable ways:

- **Order.** Chromium calls `thread_policy_set` three times as `THREAD_EXTENDED_POLICY` →
  `THREAD_PRECEDENCE_POLICY` → `THREAD_TIME_CONSTRAINT_POLICY`. ScreenExtend applies precedence
  *after* the time-constraint policy (`pipeline.rs:135`), and never sets the extended policy at all.
- **Budget.** Chromium's guaranteed duty cycle is `kGuaranteedAudioDutyCycle = 0.75` with
  `kMaxAudioDutyCycle = 0.85`, i.e. `computation ≈ 0.75 × period`, `constraint ≈ 0.85 × period`, and
  it floors the period at `kTimeQuantum = 2.9 ms` with the comment *"The lower the period, the more
  CPU frequency may go up"*. ScreenExtend asks for `computation = period / 8` — **12.5 %** — and
  clamps `constraint` to 4 ms (`qos.rs:35-39`). At 60 fps that is a 2.08 ms guarantee for a job that
  is a whole 1080p frame. A time-constraint thread that overruns its declared computation is
  descheduled by the Mach scheduler, so the effect of the current numbers under load is likely the
  opposite of the intent.

**The `os_workgroup` is not merely undriven — the type it creates is documented as a no-op.**
`os/workgroup_interval.h` in libdispatch
([source](https://github.com/apple-oss-distributions/libdispatch/blob/main/os/workgroup_interval.h)):

> "During the first instance of this API, **the only supported interval workgroups are for audio
> workloads.** Please refer to the AudioToolbox framework for more information."

and `os/workgroup_interval_private.h`
([source](https://github.com/apple-oss-distributions/libdispatch/blob/main/os/workgroup_interval_private.h)):

> "Clients need the **`com.apple.private.kernel.work-interval`** entitlement to create all
> workgroups types listed below except the following: `OS_WORKGROUP_INTERVAL_TYPE_DEFAULT`,
> `OS_WORKGROUP_INTERVAL_TYPE_CA_CLIENT`, `OS_WORKGROUP_INTERVAL_TYPE_AUDIO_CLIENT`."
> … "**Setting type `OS_WORKGROUP_INTERVAL_TYPE_DEFAULT` on an `os_workgroup_interval_t` is a
> no-op.**"

The interesting types for a compositor-driven encoder — `COREANIMATION`, `CA_RENDER_SERVER`,
`COREMEDIA`, `FRAME_COMPOSITOR` — are all behind that private entitlement. `qos.rs:138-142` passes
a NULL attr, which is the DEFAULT type, which the header calls a no-op; and it then never calls
`os_workgroup_interval_start`/`_finish`, without which there is no start time, no deadline and
nothing for the scheduler to act on (`os_workgroup_interval_create`: *"This object tracks a
repeatable workload characterized by a start time, end time and targeted deadline"*). The
`[qos] joined real-time encode workgroup` log line at `qos.rs:157` therefore reports a guarantee
that was never requested and could not have been granted. The interval functions are macOS
**10.16 / 11.0** in any case, so the whole facility is absent on the dev box. The only sanctioned
route for a non-entitled app is `os_workgroup_interval_create_with_workload_id` (macOS 14+) with a
*system-defined* workload identifier, or `AudioWorkIntervalCreate` — neither of which describes a
video encoder. **Delete it.**

**Display-link pacing is the wrong shape for this pipeline.** Both capture backends are already
event-driven by WindowServer's own compose cadence — a `CVDisplayLink` on the virtual display would
add a second clock that can only be in phase by luck, and `CVDisplayLink` is itself deprecated in
the 15.0 SDK. The real pacing defect is internal and already filed: the encode loop polls with a
4 ms timeout (`pipeline.rs:169`, `:179`) instead of blocking on frame arrival
(`speculative-vt-encode-loop-4ms-idle-and-wake-token-race.md`).

**App Nap and power** are close to correct and cost nothing to finish: `activity.rs:129` uses
`UserInitiated | LatencyCritical` per session, but the *process-wide* activity (`tuning.rs:23-25`)
uses `UserInitiated | AutomaticTerminationDisabled | SuddenTerminationDisabled` without
`LatencyCritical`, so timer coalescing is only suppressed while a session activity is live. The two
files are near-duplicates and should be merged. The `IOPMAssertion` pair in `power.rs:63`
(`PreventUserIdleSystemSleep` + `PreventUserIdleDisplaySleep`) is the right pair — display sleep
must be prevented because WindowServer stops compositing an asleep display, which stops the capture
stream at its source.

### 2.6 `CGVirtualDisplay` from 10.14 to 26.4

The private CoreGraphics classes were dumped from the **macOS 26.4** CoreGraphics binary
([thatmarcel/macOS-26.4-headers](https://github.com/thatmarcel/macOS-26.4-headers/tree/main/headers/CoreGraphics))
and compared against the **10.14.6** class-dump
([Chr0nicT/macOS-Headers-10.14.6-Mojave](https://github.com/Chr0nicT/macOS-Headers-10.14.6-Mojave)).
Three things follow.

**(a) The API survives intact to the top of the supported range.** `CGVirtualDisplay`,
`CGVirtualDisplayDescriptor`, `CGVirtualDisplaySettings` and `CGVirtualDisplayMode` all still exist
in 26.4 with `initWithDescriptor:` / `applySettings:` unchanged. There is no newer or more public
replacement — every product in this space (BetterDisplay, DeskPad, OpenDisplay, SimpleDisplay,
FreeDisplay, the several `node-mac-virtual-display` forks) is on these same four classes. Nothing
in this report proposes moving off them.

**(b) `CGVirtualDisplaySettings` grew three properties that ScreenExtend does not set.** 10.14.6
had exactly `modes` and `hiDPI`. 26.4 has:

```objc
@property (nonatomic) double        refreshDeadline;
@property (nonatomic) _Bool         isReference;
@property (retain, nonatomic) NSArray *modes;
@property (nonatomic) unsigned int  hiDPI;
@property (nonatomic) unsigned int  rotation;
```

and `CGVirtualDisplay` carries a matching `double _refreshDeadline` ivar alongside `_client` and the
three Mach port fields. **`refreshDeadline` is the only latency-named knob anywhere on the virtual
display**, and no open-source consumer found sets it. `rotation` is a first-class orientation
control, which is relevant because ScreenExtend currently expresses orientation by swapping width
and height into a transposed mode (`pipeline.rs:543`). `CGVirtualDisplayMode` likewise gained
`transferFunction` and `initWithWidth:height:refreshRate:transferFunction:`, which is the colour
knob R8 wants on the display side. All three must be reached with `respondsToSelector:` — they do
not exist on 10.14/10.15.

**(c) The mode array has always been an array, and macOS throws its ladder away.**
`CGVirtualDisplaySettings.modes` is an `NSArray` in the 10.14.6 dump, so registering the whole DPR
ladder at create time has been possible on every OS ScreenExtend supports.
`streamer/server.rs:926` computes exactly that ladder (`dpr_mode_ladder`) and passes it to
`create_display_with_modes` — but the trait's default implementation at
`streamer/session.rs:638-648` is

```rust
fn create_display_with_modes(&self, name: String, width: u32, height: u32,
                             refresh_rate: u32, extra_modes: &[(u32, u32)]) -> Result<u32, String> {
    let _ = extra_modes;
    self.create_display(name, width, height, refresh_rate)
}
```

and only `windows_utils/virtual_display.rs:63-74` overrides it. macOS therefore registers **one**
mode (`virtual_display.rs:158`, `:217`) and every later DPR, scale or orientation change has to go
through `reconfigure_display` → `applySettings:` → `activate_mode`, which polls
`CGDisplayCopyAllDisplayModes` at 50 ms intervals for up to **1.5 s** waiting for the new mode to
appear and then polls `CGDisplayPixelsWide/High` for up to another **1.5 s** waiting for the
geometry to settle (`virtual_display.rs:230-300`). That is a designed-in stall of up to three
seconds on a code path the client can trigger by rotating a phone. This is the same lesson the
Windows side already learned and recorded in `windows-idd-mode-registration` — it simply was never
carried across.

**Refresh cadence.** WindowServer composites a display at that display's mode refresh rate, so the
`refreshRate` handed to `CGVirtualDisplayMode` is the ceiling on capture rate, and the reverse-
engineered header notes carried by OpenDisplay annotate the class as capped at 60 Hz. Combined with
`major-vt-encoder-fps-floored-at-60.md` — which floors `fps` at 60 even for a 30 Hz client — the
practical situation on macOS is that `max_fps`'s 15–500 range has one reachable value, and the
floor guarantees the host does 60 Hz of capture and encode work even when the user explicitly asked
for 30.

---

## 3. How others do it

| Project | Capture | Key settings | Notable |
|---|---|---|---|
| **OpenDisplay** ([repo](https://github.com/peetzweg/opendisplay)) — the closest twin: `CGVirtualDisplay` → SCK → VideoToolbox H.264 → TCP to an iPad/iPhone/Mac | SCK, macOS 14+ sender | `queueDepth = 8`; `minimumFrameInterval = 1/120` on a 60 Hz display; `pixelFormat = 420v`; `RealTime = true`; `AllowFrameReordering = false`; High AutoLevel; `MaxKeyFrameInterval = 3600` / `Duration = 60`; `MaxFrameDelayCount = 0`; `PrioritizeEncodingSpeedOverQuality = true`; **no `DataRateLimits`**; `EnableLowLatencyRateControl` with a retry-without fallback | `maxPendingEncodes = 1` in-flight cap; `maxPendingSends = 3`; TCP + `TCP_NODELAY`; ~18 Mb/s top preset; private-API header notes `CGVirtualDisplay` is **capped at 60 Hz** |
| **libwebrtc / Chromium** ([`screen_capturer_sck.mm`](https://github.com/webrtc-mirror/webrtc/blob/main/modules/desktop_capture/mac/screen_capturer_sck.mm)) | SCK, gated at **macOS 14.0** | `pixelFormat = BGRA`; `colorSpaceName = kCGColorSpaceSRGB`; `captureResolution = Nominal`; `minimumFrameInterval = kCMTimeZero` unless capped; **queueDepth left at default** | Accepts `Complete || Started`; reads `DirtyRects`, `BoundingRect`, `ContentRect`, `ScaleFactor`, `ContentScale`, `PresenterOverlayContentRect`; notes SCK attaches a non-empty dirty-rects list to *every* frame even when nothing changed |
| **OBS Studio** (`mac-display-capture.m`) | CGDisplayStream | `{ SourceRect, QueueDepth: 5, ShowCursor }` — **no `minimumFrameTime`**; `'BGRA'` | `CFRetain` **+ `IOSurfaceIncrementUseCount`** on the new surface, `DecrementUseCount` + `CFRelease` on the previous; logs `CGDisplayStreamUpdateGetDropCount` every delivery |
| **OBS Studio** (SCK, PR [#11896](https://github.com/obsproject/obs-studio/pull/11896)) | SCK | `minimumFrameInterval` = target minus a **10 %** buffer | Documents macOS defaulting the property to 1/60 regardless of display refresh |
| **OBS Studio** (VideoToolbox encoder) | — | `RealTime = kCFBooleanFalse`; CBR only on macOS 13 **and** Apple silicon | [#5840](https://github.com/obsproject/obs-studio/issues/5840): `RealTime = true` + bitrate limiting caused heavy frame drops on M1 |
| **Sunshine** (`av_video.m`) | **`AVCaptureScreenInput`** (not CGDisplayStream) | `kCVPixelFormatType_32BGRA`; serial queue at `QOS_CLASS_USER_INITIATED` | Hardcodes `PrioritizeEncodingSpeedOverQuality = 1` for all VT codecs. **Note:** CGDisplayStream *"typically uses about one-fifth of the CPU time to capture the same content at 60 fps as `AVCaptureScreenInput` does"* ([RetroClip](https://www.realartists.com/blog/retroclip-instant-replay-for-your-mac.html)) — AVFoundation screen capture is not an upgrade |
| **Firefox** (`AppleVTEncoder.cpp`) | — | `MaxFrameDelayCount = 0` for realtime, `kVTUnlimitedFrameDelayCount` otherwise | |
| **FFmpeg** (`videotoolboxenc.c`) | — | never sets `MaxFrameDelayCount`; HEVC low-latency only on ARM64 | Carries reverse-engineered CFString literals for `dlsym` fallback |
| **BetterDisplay / DeskPad / FreeDisplay / SimpleDisplay** | — | all use the same private `CGVirtualDisplay` classes | No newer or better virtual-display API exists; every product is on the same private API — still present unchanged in the macOS **26.4** CoreGraphics dump — and it broke WindowServer on Sequoia betas ([BetterDisplay #3198](https://github.com/waydabber/BetterDisplay/issues/3198)) |
| **Chromium** ([`platform_thread_apple.mm`](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/base/threading/platform_thread_apple.mm)) — real-time threads | — | `THREAD_EXTENDED_POLICY` → `THREAD_PRECEDENCE_POLICY` → `THREAD_TIME_CONSTRAINT_POLICY`, in that order | `computation ≈ 0.75 × period`, `constraint ≈ 0.85 × period`, period floored at 2.9 ms (*"The lower the period, the more CPU frequency may go up"*). ScreenExtend asks for `period/8` = 12.5 %, in the reverse order, after a QoS class |

The single most useful comparison is OpenDisplay, because it is the same product built by someone
else and its comments carry measurements. Its settings differ from ScreenExtend's in six places, and
in five of those ScreenExtend is on the wrong side.

---

## 4. What ScreenExtend does today

**Capture selection** — `mod.rs:116-147`. `screencapturekit_available()` (`mod.rs:79-82`) is
`macos_at_least(12, 3)`, which forks `sw_vers -productVersion` on **every call** (`mod.rs:61-77`).
SCK is tried first when that is true, CGDisplayStream on any error. TCC is probed with SCK when
available and otherwise with a real 64×64 CGDisplayStream (`mod.rs:107-114`, `cgds.rs:212-262`).

**CGDisplayStream** — `cgds.rs`. Properties at `cgds.rs:66-100`: `minimumFrameTime = (1/fps)·0.95`,
`queueDepth = 2`, `showCursor = true`, `preserveAspectRatio = false`, `YCbCrMatrix = 709`. Delivery
queue is a serial queue targeting the global `UserInteractive` queue (`cgds.rs:162-165`). In the
handler (`cgds.rs:104-157`): frames whose status is not `FrameComplete` are dropped; frames whose
`DirtyRects` count is zero are dropped (`:116-126`); the surface is `CFRetain`ed (`:134`) **without**
`IOSurfaceIncrementUseCount`; a `CVPixelBuffer` is created around it (`:136`); the frame is stamped
`arrived_mach: mach_now(), captured_at: Instant::now()` (`:148-149`) and published. The delivered
`displayTime` is bound as `_display_time` and discarded (`:172`). `CGDisplayStreamUpdateGetDropCount`
is never called.

**ScreenCaptureKit** — `sck.rs`. Configuration at `sck.rs:552-577`: `width`, `height`,
`minimumFrameInterval = 1/fps`, `pixelFormat` (420f), `queueDepth = 3`, `showsCursor = true`,
`capturesAudio = false`. Nothing else — no `colorMatrix`, no `colorSpaceName`, no `sourceRect`, no
`captureResolution`, no `preservesAspectRatio`. Delivery queue is serial, targeting global
`UserInteractive` (`sck.rs:348-351`). The delegate (`sck.rs:585-625`) filters to
`SC_FRAME_STATUS_COMPLETE = 0` only (`:87-89`, `:655-660`) and skips frames with a present-and-empty
dirty-rects attachment (`:662-675`). `publish_sample` (`:678-723`) pulls the `IOSurface` out of the
sample's pixel buffer, re-wraps it, and stamps `Instant::now()`; the sample PTS and
`SCStreamFrameInfoDisplayTime` are never read. All SCK classes come from `AnyClass::get` and all
constants from `dlsym` — the dyld discipline here is correct and should be the model for the
CGDisplayStream file.

**Encoder** — `encoder.rs`. Spec ladder at `:73-93`: `(EnableLowLatencyRateControl + RequireHW)`
when `macos_at_least(11, 0)` — **the correct version is 11.3** — then `(RequireHW)`, then
`(EnableHW)`. Properties at `:145-236`: `RealTime = true`, `AllowFrameReordering = false`,
profile (Constrained variants preferred via `dlsym`), 709 primaries/transfer/matrix,
`ExpectedFrameRate = fps`, `MaxKeyFrameInterval = fps·60`, **`MaxKeyFrameIntervalDuration = 10.0`**,
`MaxFrameDelayCount = 1`, `MaximizePowerEfficiency = false`,
`PrioritizeEncodingSpeedOverQuality = true`. Rate control at `:239-266`: `AverageBitRate` **plus**
`DataRateLimits` at 1.5× over a **0.5 s** window, plus `MaxAllowedFrameQP = 45`. `set_bitrate`
(`:333-350`) re-applies the pair. `log_hw_acceleration` (`:355-370`) reads
`UsingHardwareAcceleratedVideoEncoder` and only prints it. `EncoderID`, `UsingGPURegistryID` and
`NumberOfPendingFrames` are never read. The `kVT*` constants at `:18-27` are **link-time** imports
from `objc2-video-toolbox` (all 10.8/10.9 keys, so safe on the floor); everything newer goes through
`vt_optional_cfstring`/`set_optional_*` (`:566-590`), whose `bool` results are discarded.

**Pipeline** — `pipeline.rs`. One `videotoolbox-encode` thread (`:105-119`) configured
`USER_INTERACTIVE` → `THREAD_TIME_CONSTRAINT` → `THREAD_PRECEDENCE` → affinity → workgroup join
(`:133-137`), plus a `videotoolbox-drain` thread (`:141-171`). The loop waits 4 ms, takes the latest
frame, submits it, and otherwise re-encodes the pinned `last_frame` every 200 ms as a keepalive
(`:167-206`). `submit_one` pushes onto an unbounded `VecDeque<Instant>` (`:130`, `:222-236`); nothing
counts frames in flight. On non-low-latency sessions the keepalive calls the **blocking**
`complete_pending` on the real-time thread (`:209-215`). Capture is requested at the already-scaled
encode dimensions (`:253-258`, `:278-283`) — WindowServer does the scale and the RGB→YCbCr
conversion, and no `sourceRect`/`destinationRect` is set.

**Scheduling** — `qos.rs`, `tuning.rs`, `power.rs`, `activity.rs`. `pthread_set_qos_class_self_np`
with `USER_INTERACTIVE` (0x21), then `thread_policy_set` with `THREAD_TIME_CONSTRAINT_POLICY`
(period = frame duration, computation = period/8, constraint clamped to ≤ 4 ms),
`THREAD_PRECEDENCE_POLICY` importance 63, and `THREAD_AFFINITY_POLICY` tag 1 on x86_64 only. Every
return code is discarded (`qos.rs:21`, `:47`, `:63`, `:82`). `FrameWorkgroup::join` (`qos.rs:128-165`)
dlsyms `os_workgroup_interval_create`/`join`/`leave`, creates a workgroup with a **NULL attribute**
and never calls `os_workgroup_interval_start`/`_finish` — the period parameter is `_period_ns`,
unused — then logs *"joined real-time encode workgroup"*. `tuning.rs` begins a process-wide
`NSActivity` with `UserInitiated | AutomaticTerminationDisabled | SuddenTerminationDisabled` and
calls `setpriority(PRIO_PROCESS, 0, -10)`. `activity.rs` begins a second `NSActivity` with
`UserInitiated | LatencyCritical` per session. `power.rs` takes `PreventUserIdleSystemSleep` and
`PreventUserIdleDisplaySleep` IOPM assertions.

**Virtual display** — `virtual_display.rs`. The private `CGVirtualDisplay*` classes are declared with
`extern_class!` (objc2 resolves at runtime, so a Mac without them still loads) and the tier is chosen
by `AnyClass::get` (`:334-361`) — correct probing. The descriptor gets a fixed
`productID = 0x1234` / `vendorID = 0x3456`, an incrementing serial, a 96-DPI physical size, and the
global high-priority dispatch queue. `create_cg_virtual_display` (`:162-228`) applies **one** mode;
`reconfigure_display` (`:381-433`) applies a new settings object and then `activate_mode`
(`:230-300`), which polls `CGDisplayCopyAllDisplayModes` for up to 1.5 s and then waits up to another
1.5 s for the geometry to settle. HiDPI is a binary flag driven by a 150 % threshold
(`pipeline.rs:533`, `:522-540`). No `terminationHandler` is set. Both `setModes:` call sites pass a
one-element array, and the shared `create_display_with_modes` hook that carries the DPR ladder is
left at its no-op default (`streamer/session.rs:638-648`, `let _ = extra_modes;`) — only Windows
overrides it. `refreshDeadline`, `rotation` and `transferFunction` are never touched. The
descriptor's queue is the global `DISPATCH_QUEUE_PRIORITY_HIGH` queue (`virtual_display.rs:172`),
which is `USER_INITIATED`, not `USER_INTERACTIVE`.

**Process/bundle configuration** — `src-tauri/Info.plist` declares only
`NSAudioCaptureUsageDescription` and the `NSAppTransportSecurity` dictionary.
`NSSupportsAutomaticGraphicsSwitching` is absent, `NSProcessInfo.thermalState` is never read, and
`GPURegistryID` appears nowhere in the crate.

---

## 5. Recommendations

Ranked by expected latency (or correctness) delta ÷ implementation cost. Every one is 10.15-safe.
Findings files are named after each.

### R1 — Fix the IOSurface use-count contract, and raise both queue depths, in one change

- **API:** `IOSurfaceIncrementUseCount` / `IOSurfaceDecrementUseCount` (IOSurface, macOS 10.6 — on
  the floor, already linked via `objc2-io-surface`); `kCGDisplayStreamQueueDepth` 2 → 5 or omitted
  (default 3); SCK `queueDepth` 3 → 8.
- **Probe:** none needed — floor API on every supported OS.
- **Expected win:** correctness first. Today WindowServer may composite into a buffer VideoToolbox is
  reading, which produces tearing and stale bands that worsen under load. After the fix, the deeper
  queue is what keeps the capture rate at the display refresh instead of at our release rate.
  OpenDisplay's `queueDepth = 8` comment attributes ~13 ms of hold to the encoder alone.
- **Order matters:** increment-without-depth turns a corruption bug into a starvation bug.
- **Measure:** `CGDisplayStreamUpdateGetDropCount` before / after-increment / after-depth. Expected
  signature: near zero → spike → near zero. Plus a visual tear check on a moving high-contrast
  pattern.
- Findings: `blocker-cgds-iosurface-use-count-never-incremented.md`,
  `blocker-cgds-queue-depth-starvation.md` (updated),
  `major-sck-queue-depth-3-contradicts-apples-documented-default.md`.

### R2 — Read `CGDisplayStreamUpdateGetDropCount`, `EncoderID` and `NumberOfPendingFrames`

Three numbers, all available on the 10.15 floor, none currently read, and together they decide which
of every other recommendation matters.

- `CGDisplayStreamUpdateGetDropCount` (10.8) — "is the client keeping up with WindowServer".
- `kVTCompressionPropertyKey_EncoderID` (10.13) — *which* VideoToolbox encoder was selected. Forum
  748918 measured a 4–16 frame hold on `ave.avc` vs one-in/one-out on `gva` under identical settings.
  At 60 fps that is 67–267 ms hiding inside a single property nobody reads.
- `kVTCompressionPropertyKey_NumberOfPendingFrames` (10.8, read-only) — live frames in flight.
- **Expected win:** 0 ms directly; it is the prerequisite for trusting R1 and R4–R7, and it may
  short-circuit the whole investigation if the answer is "this Mac got the 16-frame encoder".
- **Cost:** an hour. `VTSessionCopyProperty` is already declared at `encoder.rs:546`.
- Findings: `major-cgds-drop-count-never-read.md`,
  `major-vt-encoder-implementation-is-never-read-back-and-decides-a-4-to-16-frame-hold.md`.

### R3 — Stop throttling the capture: drop `minimumFrameTime`, set SCK's interval to `kCMTimeZero`

- **API:** omit `kCGDisplayStreamMinimumFrameTime` (documented default **zero**); set SCK
  `minimumFrameInterval = kCMTimeZero` (documented as *"capture at display's native refresh rate"*),
  or a *shortened* interval if a deliberate cap is wanted.
- **Expected win:** on CGDisplayStream, ~8 ms mean / ~17 ms worst case at 60 fps, on every frame. On
  SCK, additionally the frames the beat frequency is currently eating — OpenDisplay measured
  **51 fps instead of 60**, a 15 % loss, from setting exactly `1/60`.
- **Cost:** delete one line; change one constant.
- **Measure:** delivered fps vs wall clock, drop count, and the existing
  `pipeline::tests::end_to_end_latency` histogram. Record CPU in both runs.
- Findings: `minor-cgds-minimum-frame-time-coalescing.md` (updated, severity → major),
  `major-sck-minimum-frame-interval-exactly-one-over-fps-drops-frames.md`.

### R4 — Cap frames in flight ourselves; stop asking `MaxFrameDelayCount` to do it

- **API:** an `AtomicUsize` incremented at `submit_one` and decremented in the drain thread, with
  submission skipped while ≥ 1; `MaxFrameDelayCount` set to `kVTUnlimitedFrameDelayCount` (−1) or
  left unset.
- **Why:** `MaxFrameDelayCount = M` is a *blocking* contract — frame N−M must be emitted before
  `EncodeFrame(N)` returns — and the caller is the `THREAD_TIME_CONSTRAINT` thread. Meanwhile nothing
  bounds VideoToolbox's own input queue; OpenDisplay measured **~8 frames queued** when overfed, and
  ScreenExtend's 4 ms loop with a 60 fps floor can overfeed.
- **Expected win:** removes (max_in_flight − 1) frame periods from p99 with no loss in delivered fps,
  because the frames that stop being submitted were stale. Potentially 50–130 ms on a slow host.
- **Measure:** in-flight max, `pending_ts` depth distribution, and p99 capture→encoded latency.
- Finding: `major-vt-no-cap-on-frames-in-flight-inside-the-encoder.md`.

### R5 — Remove `DataRateLimits`; stop emitting a periodic IDR

- **API:** drop `kVTCompressionPropertyKey_DataRateLimits` from `configure_rate_control` and
  `set_bitrate`; raise `MaxKeyFrameIntervalDuration` far above 10 s.
- **Why:** Chromium documents that `DataRateLimits` is **incompatible with
  `EnableLowLatencyRateControl`** (undocumented by Apple) and that *"even in non low latency mode
  `DataRateLimits` tends to make the encoder undershoot set bitrate"*. OBS #5840 found
  `RealTime = true` plus bitrate limiting caused heavy frame drops on M1. The 0.5 s / 1.5× window is
  also being asked to absorb an IDR we chose to create every 10 s. The loss-based BWE already owns
  peak control.
- **Expected win:** removes a ~10 s-cadence quality/timing hiccup and lets the encoder actually spend
  the bitrate the controller allocated. No comparable product ships `DataRateLimits`.
- **Measure:** per-frame encoded size and submit→emit dwell across a 10 s scroll, correlated against
  the IDR cadence; `probe_bitrate` already prints target vs achieved.
- Findings: `major-vt-datarate-limits-imposes-a-hard-cap-on-a-realtime-screen-encoder.md`,
  `minor-vt-periodic-idr-every-10-seconds.md`.

### R6 — Carry the real capture time, not the callback arrival time

- **API:** CGDisplayStream's `displayTime` (already delivered, discarded at `cgds.rs:172`); SCK's
  `SCStreamFrameInfoDisplayTime` attachment, or `CMSampleBufferGetPresentationTimeStamp` plus
  `SCStream.synchronizationClock` (13.0+) to convert cleanly.
- **Why:** both are documented as *mach absolute time of when the frame was displayed by the window
  server*. Stamping `Instant::now()` at callback entry folds the whole delivery interval into "host
  capture time", so the client's `videoDelayEmaMs` under-reports real glass-to-glass and the A/V
  anchor drifts against audio, which uses a genuine capture instant.
- **Expected win:** 0 ms directly. It makes every other number in this report trustworthy, and
  `arrived − displayTime` is itself the "is WindowServer or our queue the bottleneck" metric.
- Finding: `major-cgds-capture-timestamp-is-callback-arrival-not-display-time.md` (existing).

### R7 — Accept `SCFrameStatusStarted`, and never let a new client see black

- **API:** accept status 4 as well as 0 in `frame_is_complete_and_dirty`; bypass the dirty-rect skip
  for the first frame of a session.
- **Why:** Apple defines `Started` as *"new frame that is indicated as the first frame sent after the
  stream has started"* — it carries pixels — and libwebrtc accepts it. Dropping it means a client
  joining a *static* virtual display (a fresh one with no windows is exactly that) waits for the
  first `Complete`, which may never come. The 200 ms keepalive cannot help: `last_frame` is `None`.
- **Expected win:** eliminates a class of "joined and it's black" reports; time-to-first-pixel drops
  from unbounded to one frame interval.
- Findings: `major-sck-first-frame-after-start-is-discarded.md`,
  `major-cgds-dirty-rect-skip-can-blank-a-new-client.md` (existing).

### R8 — Switch the capture pixel format to `420v` and set the colour space explicitly

- **API:** `PIXEL_FORMAT_420F` → `'420v'` (`mod.rs:58`); SCK `colorSpaceName` +`colorMatrix`;
  CGDisplayStream `kCGDisplayStreamColorSpace`.
- **Why:** there is **no** `kVTCompressionPropertyKey_FullRangeVideo` — range is carried by the input
  pixel format alone, and `YCbCrMatrix` does not write `video_full_range_flag`. Feeding `420f` while
  the VUI carries no range flag lets the client decoder assume limited range and crush blacks.
  `420v` is also what OpenDisplay picks, with the note *"420v matches the encoder's native input —
  skips a BGRA→YUV conversion inside VideoToolbox"*. Both SCK and CGDisplayStream default their
  colour space to *"the same color space as the display"* — and here that display is a
  `CGVirtualDisplay` whose primaries we set by hand in a different file.
- **Expected win:** correctness and cross-backend consistency, not milliseconds.
- Finding: `minor-sck-color-config-diverges-from-cgds.md` (updated with the primary sources).

### R9 — Replace PLI→IDR with PLI→LTR-refresh where the OS offers it

- **API:** `kVTCompressionPropertyKey_EnableLTR` (**12.0**), `…_RequireLTRAcknowledgementToken`
  (sample attachment), `kVTEncodeFrameOptionKey_AcknowledgedLTRTokens`,
  `kVTEncodeFrameOptionKey_ForceLTRRefresh` — all `dlsym`-resolvable with the existing helper.
- **Why:** an IDR at 1080p is 10–30× a P frame; on a lossy link an IDR causes the burst that causes
  the next PLI. LTR gives a P frame referencing a known-received frame instead. The built-in fallback
  is *"an IDR if no LTR frames have been acknowledged"* — i.e. exactly today's behaviour — which makes
  it unusually safe to adopt.
- **Cost:** highest in this list; needs an acknowledgement signal, which the existing control
  DataChannel can carry.
- **Caveat:** no shipping consumer of these keys was found (Chromium has no LTR code at all), so
  verify the property actually took before trusting it.
- Finding: `major-vt-ltr-frames-would-replace-idr-on-every-pli.md`.

### R10 — Prefer SCK by probing, not by `sw_vers`; keep CGDisplayStream off modern macOS

- **API:** `AnyClass::get(c"SCShareableContent")` as the selector, plus a **frame-level** probe
  (start the stream, require one frame within a deadline) before committing — the CGDisplayStream
  probe already works this way. Resolve the `kCGDisplayStream*` symbols with `dlsym` instead of
  linking them.
- **Why, both directions:** below 13.0, libwebrtc reports full-screen SCK capture is broken
  (crbug.com/40234870) and gates at 14.0 — so a 12.3 host should be able to fall back. Above 15.0,
  CGDisplayStream is obsoleted in the SDK and triggers Sequoia's privacy alert — so a modern host
  should never touch it, and today `probe_screen_recording` can run it even when SCK is used.
  Removing the link-time reference also removes a whole-binary load risk if the exports ever go.
- **Verify:** `nm -u target/release/ScreenExtend | grep -i displaystream` must return nothing.
- Findings: `major-sck-tier-enabled-at-12.3-but-known-broken-before-13.0.md`,
  `major-cgds-obsoleted-in-the-macos-15-sdk-and-triggers-sequoia-privacy-alerts.md`,
  `major-compat-mac-capabilities-gated-by-a-sw-vers-subprocess.md` (existing).

### R11 — Detect the software encoder properly and act on it

- **API:** store the read-back `UsingHardwareAcceleratedVideoEncoder`, falling back to `EncoderID`
  when it is absent (it is absent on the low-latency `rtvc` encoders); clamp encode dimensions up to
  clear the Intel hardware floor (**width ≥ 640 or height ≥ 480**, per Chromium's measured table).
- **Why:** ScreenExtend sizes the encoder from client dimensions × user `videoScale`, so a small
  client at 50 % scale falls under the Intel threshold and lands silently on a CPU encoder that
  cannot keep up at 60 fps — which looks exactly like "you must quit everything else".
- **Cost:** low. `scaled_dims` (`config.rs:15-22`) is the one place to clamp.
- Findings: `major-vt-intel-macs-refuse-hardware-h264-below-640-wide-and-480-tall.md`,
  `major-vt-silent-software-encoder.md` (existing).

### R12 — Do not build the Metal path

Recorded so it is not proposed again. `gpu.rs` is a stub and should stay one (or be deleted). The
current path is already zero-copy: WindowServer scales and converts on the GPU and vends an
IOSurface; we wrap it and hand it to VideoToolbox. A Metal scale/convert stage would ask for BGRA,
redo work the compositor already did, and add a surface allocation per frame. If measurement ever
implicates WindowServer's scaler, the cheap first move is `sourceRect`/`destinationRect` (both APIs
expose them; neither backend sets them), not a Metal pipeline.

Findings: `minor-vt-gpu-struct-is-an-empty-stub-claude-md-says-otherwise.md`,
`speculative-cgds-windowserver-does-the-downscale-with-no-source-rect.md` (existing).

### R13 — Ask VideoToolbox what it supports instead of asking what macOS version this is

- **API:** `VTCopySupportedPropertyDictionaryForEncoder(width, height, kCMVideoCodecType_H264,
  encoderSpecification, &encoderID, &supportedProperties)` — **macOS 10.13**, i.e. on the floor.
  Backed by `VTCopyVideoEncoderList` (10.8) + `kVTVideoEncoderList_IsHardwareAccelerated` /
  `_GPURegistryID` / `_InstanceLimit` (10.14) for the machine-wide picture, and
  `VTSessionCopySupportedPropertyDictionary` (10.8) for a live session.
- **Why:** it returns, before any session exists, *which encoder* will be selected for these exact
  dimensions and *which properties it honours*. That single call replaces (a) `macos_at_least(11,0)`
  gating `EnableLowLatencyRateControl` (`encoder.rs:74`, and the correct version is 11.3 anyway),
  (b) the `dlsym`-then-set-and-discard-the-result pattern in `set_optional_*`
  (`encoder.rs:566-590`), and (c) Chromium's hand-measured Intel minimum-resolution table that R11
  proposes to copy. It is the "probe the capability, never the version" doctrine applied to the
  encoder.
- **Probe:** the function itself is the probe; `dlsym` it so the 10.13 symbol is never a link-time
  reference.
- **Expected win:** 0 ms directly, but it removes an entire class of silent misconfiguration and
  makes R5, R9 and R11 verifiable rather than hopeful.
- **Measure:** log the returned encoder ID and the supported-property key list once per session at
  startup; diff it across the dev box and any 11/12/13/14 machine that becomes available.
- Finding: `major-vt-supported-property-dictionary-never-queried.md`.

### R14 — Detect hardware-encoder contention and say so, instead of degrading silently

- **API:** `kVTVideoEncoderList_InstanceLimit` (10.14) to learn that the encoder is capped at all;
  `UsingHardwareAcceleratedVideoEncoder` + `EncoderID` (R2/R11) to learn what this session actually
  got; a `-12902`-class create failure on the `RequireHW` rung to learn that it was busy.
- **Why:** this is the **documented** cause of the reported symptom. Apple's own text for
  `RequireHardwareAcceleratedVideoEncoder` lists *"the hardware encoding resources on the machine
  are busy"* as a common failure, and `InstanceLimit` exists to advertise that the encoder is
  *"a scarce resource which may potentially be unavailable"*. ScreenExtend creates one
  `VTCompressionSession` per client and falls through to a software rung without a word
  (`encoder.rs:73-93`), so both "another app took the encoder" and "you have too many clients on
  this Mac" present to the user as unexplained badness that quitting apps happens to fix.
- **Expected win:** no latency, large support win — and it converts the single most common bad
  experience into an actionable message. It also bounds the multi-client story: on a Mac whose
  encoder is instance-limited, the Nth client should be told, not silently served by libx264-grade
  software encode.
- **Measure:** count sessions that land on each ladder rung over a week of use; open a video call
  in another app mid-stream and confirm the log and the UI both report the demotion.
- Finding: `major-vt-hardware-encoder-is-a-scarce-machine-wide-resource-with-no-detection.md`.

### R15 — Stop forcing the discrete GPU on, and pin the encoder to the display's GPU

Two halves of the same Intel-Mac problem; ship them together.

- **API (a):** add `NSSupportsAutomaticGraphicsSwitching = true` to `src-tauri/Info.plist`
  (macOS 10.7+, no runtime probe needed — it is a static Info.plist key).
- **API (b):** `CGDirectDisplayCopyCurrentMetalDevice(displayID)` (CoreGraphics, **10.11**) →
  `[MTLDevice registryID]` by `msg_send!` → pass as
  `kVTVideoEncoderSpecification_PreferredEncoderGPURegistryID` (**10.14**) in the encoder spec.
  Prefer, never Require — Required would defeat the fallback ladder.
- **Why:** QA1734 says an app without the key permanently switches a dual-GPU MacBook Pro to the
  discrete GPU on first GL/CA context creation, "won't switch back until the application quits" —
  and ScreenExtend is a WKWebView app. Sustained dGPU power on a 2016–2019 chassis that is also
  encoding 1080p60 is a thermal-throttle generator, and thermal throttling looks exactly like the
  reported symptom. (b) then ensures the encoder is picked on whichever GPU is actually driving the
  virtual display, so the IOSurface does not cross the bridge every frame.
- **Trap:** `UsingGPURegistryID` (10.15) returns **NULL for both a built-in-GPU hardware encoder and
  a software encoder**, so do not use it to detect the software fallback — that is R11's job.
- **Measure:** on a dual-GPU Mac, `pmset -g` / `system_profiler SPDisplaysDataType` for which GPU is
  active before and after launch; `powermetrics` for package power and thermal pressure over a
  10-minute stream; p99 capture→encoded before and after pinning.
- Findings: `major-build-mac-app-forces-the-discrete-gpu-for-its-whole-lifetime.md`,
  `major-vt-encoder-not-pinned-to-the-gpu-driving-the-virtual-display.md`.

### R16 — Delete the workgroup; fix the order and the budget of what remains

- **Why delete:** `os/workgroup_interval.h` says *"the only supported interval workgroups are for
  audio workloads"*; the private header says every type other than `DEFAULT`, `CA_CLIENT` and
  `AUDIO_CLIENT` needs the `com.apple.private.kernel.work-interval` entitlement and that setting
  `DEFAULT` *"is a no-op"*. `qos.rs:138-142` passes a NULL attr — the DEFAULT type — and never
  starts an interval, so the object carries no start time and no deadline. The whole facility is
  macOS 11.0+ anyway, so it is absent on the dev box. It costs four `dlsym`s, a 64-byte boxed token,
  and one log line that claims a real-time guarantee nobody asked for.
- **What to fix instead:** reorder to Chromium's sequence (`THREAD_EXTENDED_POLICY` →
  `THREAD_PRECEDENCE_POLICY` → `THREAD_TIME_CONSTRAINT_POLICY`), drop the QoS-class call on any
  thread that then takes a Mach policy (`pthread/qos.h`: a conflicting call leaves the thread
  *"permanently opted-out of the QOS class system"*), and size `computation` from a measured p95
  encode time rather than `period / 8` — Chromium's guaranteed duty cycle for the analogous thread
  is **0.75**, six times ScreenExtend's 0.125.
- **Expected win:** the current configuration is at best inert and at worst self-demoting under
  load, which is the regime the user is complaining about. This is cheap and the returns should be
  read back with `thread_policy_get`, not assumed.
- Findings: `major-vt-workgroup-created-but-never-driven.md` (existing — the primary sources above
  settle it in favour of option (a), delete),
  `major-vt-qos-thread-policy-returns-ignored.md` (existing — now with Apple's and Chromium's
  numbers), `minor-vt-setpriority-fails-for-unprivileged-users.md` (existing).

### R17 — Register the DPR mode ladder on macOS at create time

- **API:** `CGVirtualDisplaySettings.modes` has been an `NSArray` since 10.14; override
  `create_display_with_modes` in `macos_utils::virtual_display` instead of inheriting the no-op
  default at `streamer/session.rs:638-648`, and pass `dpr_mode_ladder`'s full list plus each
  transpose into `setModes:` at creation.
- **Why:** today macOS registers exactly one mode (`virtual_display.rs:158`, `:217`), so every DPR,
  video-scale or orientation change goes through `reconfigure_display` → `applySettings:` →
  `activate_mode`, which polls for the new mode for up to **1.5 s** and then waits for the geometry
  to settle for up to another **1.5 s** (`virtual_display.rs:230-300`). A client rotating a phone
  can trigger a three-second stall. Windows already does this correctly
  (`windows_utils/virtual_display.rs:63-74`); the shared hook exists and macOS simply never
  implemented it.
- **Expected win:** up to ~3 s off every reconfiguration, and removal of the "no WxH mode available"
  failure path.
- **Measure:** time `reconfigure_display` across a scale change and an orientation flip, before and
  after; count how often `activate_mode`'s find-loop iterates more than once.
- Finding: `major-vdisplay-mac-discards-the-dpr-mode-ladder-and-always-creates-at-1x.md` (existing —
  updated with the 10.14.6 and 26.4 class-dumps, which close its open question by proving
  `settings.modes` has been an `NSArray` on every macOS in range, so multi-mode registration at
  create time is supported rather than hoped for).

### R18 — Try `CGVirtualDisplaySettings.refreshDeadline`

- **API:** `-[CGVirtualDisplaySettings setRefreshDeadline:]` (a `double`; absent in the 10.14.6
  class-dump, present in the macOS 26.4 dump alongside a `double _refreshDeadline` ivar on
  `CGVirtualDisplay`). Guard with `respondsToSelector:`, never a version check.
- **Why:** it is the only latency-named property on the virtual display, it sits on the WindowServer
  side of the boundary — the side this pipeline cannot otherwise influence — and no open-source
  consumer of these classes was found setting it. Also worth probing at the same time:
  `setRotation:` as a replacement for the transposed-mode trick, and
  `CGVirtualDisplayMode.initWithWidth:height:refreshRate:transferFunction:` for R8's colour work.
- **Cost:** low. **Confidence: low** — the semantics are unpublished, which is why this is ranked
  last and marked speculative rather than folded into R17.
- **Measure:** `displayTime → callback arrival` (R6's metric) with the property unset vs set to one
  frame period vs a fraction of one; watch for WindowServer instability, which is the known failure
  mode of this class family on recent macOS.
- Finding: `speculative-vd-refresh-deadline-never-set.md`.

---

## 6. Verification plan

**Measurable on the macOS 10.15.8 Intel dev box** (most of this list — the CGDisplayStream path is
the dev box's only path, which makes it the *right* box for this territory):

1. **Encoder identity and dwell first.** Print `EncoderID`, `UsingHardwareAcceleratedVideoEncoder`,
   `UsingGPURegistryID` and per-frame submit→emit latency from
   `pipeline::tests::end_to_end_latency` (`pipeline.rs:551-660`). If dwell is 4–16 frame periods, the
   encoder is the story and R1/R3 are noise by comparison.
2. **Drop count baseline**, then the three-point sequence for R1 (near zero → spike after adding
   `IOSurfaceIncrementUseCount` at `queueDepth = 2` → near zero after raising the depth).
3. **`minimumFrameTime` A/B** (R3): delivered fps, drop count, capture→encoded p50/p99, and process
   CPU, with the key present vs omitted, on an animated screen.
4. **In-flight cap A/B** (R4): p99 capture→encoded with and without the cap; delivered fps must not
   fall.
5. **`DataRateLimits` A/B** (R5): per-frame encoded size and submit→emit dwell over a 10 s scroll,
   correlated against the 10 s IDR cadence. `probe_bitrate` (`pipeline.rs:295-352`) already prints
   target vs achieved.
6. **Hardware-encoder threshold sweep** (R11): `Encoder::new` over 320×240 / 400×300 / 568×320 /
   640×360 / 640×480 / 1280×720, printing the rung that succeeded plus the read-back properties.
   That produces this machine's real threshold table in one run.
7. **Tear check** (R1): a moving high-contrast pattern, decoded and inspected for horizontal seams
   and stale bands.
8. **Encoder capability dump** (R13/R14). One run of `VTCopySupportedPropertyDictionaryForEncoder`
   at the session's real dimensions plus `VTCopyVideoEncoderList`, printing the encoder ID, the
   supported-property key list, `IsHardwareAccelerated`, `GPURegistryID` and `InstanceLimit`. This
   is the machine's own answer to items 1 and 6 of this plan and to open questions 4 and 6, and it
   costs one function call.
9. **Contention A/B** (R14): start the stream, then start a hardware H.264 encode in another
   process (a QuickTime screen recording is enough), and confirm the ladder demotion is detected
   and reported rather than silent.
10. **Thread-policy readback** (R16): `thread_policy_get(THREAD_TIME_CONSTRAINT_POLICY)` and
    `pthread_get_qos_class_np` on the encode thread after configuration, logged once. This settles
    empirically whether `pthread/qos.h`'s "permanently opted-out" clause fires here.
11. **Reconfiguration stall** (R17): wall-clock time through `reconfigure_display` for a scale
    change and an orientation flip, before and after registering the ladder. Expect seconds → tens
    of milliseconds.

**Cannot be verified on either dev box** (Windows 11 + RTX 3080 Ti; macOS 10.15.8 Intel):

- **Everything ScreenCaptureKit.** `queueDepth` 3 vs 8, `minimumFrameInterval` `1/fps` vs
  `kCMTimeZero`, `SCFrameStatusStarted`, `SCStreamFrameInfoDisplayTime`, and the whole
  12.3-vs-13-vs-14 reliability question. Needs 12.3+, ideally three machines across that band.
- **`EnableLowLatencyRateControl`** (11.3+) and every 11/12/13-gated encoder property — LTR,
  `MaxAllowedFrameQP`/`MinAllowedFrameQP`, `ConstantBitRate`, temporal layers.
- **macOS 15/26 behaviour**: whether CGDisplayStream still functions, whether the privacy alert
  fires, whether `nm -u` still resolves.
- **Apple silicon anything.** Every OpenDisplay measurement quoted here (5.3 vs 6.1 ms, 8 frames
  queued when overfed, 51 vs 60 fps) is from Apple silicon and may not transfer to Intel.
- **`CGVirtualDisplay` on 15/26** — BetterDisplay reported WindowServer crashes on Sequoia betas with
  the same private API. `refreshDeadline` / `rotation` / `transferFunction` (R18) exist only above
  the floor and cannot be exercised on the dev box at all.
- **Dual-GPU behaviour** (R15). Neither dev box is a dual-GPU Intel MacBook Pro; the 10.15.8 box is
  Intel but the discrete-GPU switch and the cross-GPU IOSurface migration need a 2016–2019 15-inch
  or a Mac Pro to observe. The Info.plist half of R15 is free and safe regardless.
- **Encoder contention on Apple silicon** (R14) — `InstanceLimit` is reported per encoder and the
  Apple silicon media engine's cap differs from Intel Quick Sync's. The dev box will produce one
  data point, not the table.

**Two numbers to add to `docs/BASELINE.md` before any change lands:** CGDisplayStream delivered fps
and drop count over 30 s on an animated screen; and VideoToolbox `EncoderID` plus submit→emit
p50/p99 at the session's real resolution. Everything else in this report is an argument about how to
move those two.

---

## 7. Open questions

1. **Which VideoToolbox encoder implementation does the 10.15.8 Intel box get, and how deep is it?**
   Forum 748918's 4-vs-16-frame spread dwarfs every other effect in this report. R2 answers it in an
   hour and may re-order everything else. *Do this first.*
2. **Does CoreGraphics on 10.15 actually recycle a surface whose use count is 0 but whose CF retain
   count is > 1?** The header and OBS both say the counters are independent; nothing here has
   observed it. If it does not, R1's severity drops from correctness to throughput.
3. **What does crbug.com/40234870 say "broken" means** for full-screen SCK capture before 13.0 — a
   hard failure (which our fallback catches) or silent wrong content (which it does not)?
4. **Does `EnableLowLatencyRateControl` forbid Constrained Baseline?** The header and Apple's docs
   both say *"Only High profiles"*; WWDC21 presents Constrained Baseline as a low-latency feature.
   Unresolved in every source found. It matters for browser clients that advertise only `42e01f`.
5. **Is `RealTime = true` right for us?** OBS ships `false` after measuring frame drops when combined
   with bitrate limiting; libwebrtc and Chromium ship `true`. R5 removes the bitrate limiting, which
   may make the question moot — but it should be A/B'd on Intel, not assumed.
6. **Is the `ave` vs `gva` encoder split Apple-silicon vs Intel?** Forum 748918 does not say, and no
   source maps the identifiers to hardware.
7. **Does the virtual display's 60 Hz cap make the whole `max_fps = 500` configuration surface
   meaningless on macOS?** The reverse-engineered `CGVirtualDisplay` header used by every product in
   this space is annotated *"capped at 60 Hz"*. If true, `config.rs`'s clamp to `(15, 500)` and the
   floor at 60 describe a range with exactly one reachable value on macOS.
8. **No published packet capture or property dump of Sidecar's own VideoToolbox configuration
   exists.** The only route to a first-party answer is the one `docs/research/airplay-why-fast.md`
   §7 already identifies: run our own fake receiver against a real macOS sender and parse the
   resulting SPS/PPS and slice headers.
9. **How many concurrent hardware H.264 sessions does a given Mac allow?** `InstanceLimit` is a
   CFBoolean — it says *that* a cap exists, never *what* it is. ScreenExtend creates one session per
   client and simultaneous displays are deliberately uncapped, so the two policies collide somewhere
   and nothing in the tree knows where. R13's probe plus a loop that creates sessions until failure
   answers it per machine in one run.
10. **What are `CGVirtualDisplaySettings.refreshDeadline`'s units and semantics?** Seconds, mach
    ticks, or a CVTime-style value? Absent from 10.14.6, present in 26.4, undocumented, and unset by
    every open-source consumer found. R18 is the only proposal in this report resting on a guess.
11. **Does `os_workgroup_interval_create` with a NULL attr fail, or succeed and do nothing?**
    `qos.rs:129-157` reports success today, so it evidently succeeds — but the private header says
    the DEFAULT type is a no-op, so "succeeds and does nothing" is the likely answer, and it is
    consistent with the log line being the only evidence anyone has ever had that it works.
12. **Is a `CA_CLIENT` interval workgroup (one of the three non-entitled types) meaningful for a
    capture→encode loop driven by WindowServer's compose cadence?** It is the CoreAnimation *client*
    type, so on paper it describes a thread that renders in step with the compositor — which is
    close to what the encode thread is. Nobody was found using it this way, and R16 recommends
    deleting the workgroup rather than pursuing this; recorded so the option is not lost.
