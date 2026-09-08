# System-audio capture tiers and the end-to-end audio latency floor

## 1. Question

What are **all** the system-audio capture tiers available on Windows 10 (2020-era, 19041+)
through Windows 11 26xxx and on macOS 10.15 through macOS 26; what latency does each add; and
how does the end-to-end path — capture → Opus → DataChannel → decode → worklet → speaker —
get to its floor?

Sub-questions from the brief: the WASAPI engine-period floor and whether it can be lowered for a
loopback capture; process loopback; `AUTOCONVERTPCM` cost; MMCSS; the macOS Process Tap / SCK /
virtual-device tiers and their buffer sizes; Opus tuning (especially FEC on an unreliable
DataChannel); the browser playback floor; and how to probe every tier at runtime instead of by
version number.

> Note on method: this session's `WebSearch` budget was exhausted before this report started, so
> everything below is from `WebFetch` against **named primary sources** (Microsoft Learn, the
> Microsoft `Windows-classic-samples` repo, the OBS Studio and Chromium trees, opus-codec.org,
> the W3C Web Audio spec, MDN `browser-compat-data`, Apple's documentation JSON) plus the
> repository's own measured spike notes. Claims I could not reach a primary source for are
> marked **[UNVERIFIED]** and carry a concrete way to settle them.

---

## 2. Findings

### 2.1 Windows — the engine period is the whole game

The Windows shared-mode audio engine runs the **entire endpoint** at one periodicity. Microsoft
states this explicitly, and it is the single most important sentence for this report
([Low Latency Audio](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/low-latency-audio), FAQ):

> "No, by default all applications in Windows 10 and later will use 10-ms buffers to render and
> capture audio. If an application needs to use small buffers, then it needs to use the new
> AudioGraph settings or the WASAPI IAudioClient3 interface, in order to do so. **However, if one
> application requests the usage of small buffers, then the audio engine will start transferring
> audio using that particular buffer size. In that case, all applications that use the same
> endpoint and mode will automatically switch to that small buffer size.** When the low latency
> application exits, the audio engine will switch to 10-ms buffers again."

That is the workaround the brief asked about, and it is **documented, not a hack**. The loopback
capture stream does not have to be the stream that requests the small period — any stream on the
same endpoint and signal-processing mode will drag the engine down with it, and our silent render
companion is already such a stream.

**Why the loopback stream itself cannot ask.** `IAudioClient3::InitializeSharedAudioStream`
enumerates its supported flags exhaustively
([docs](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient3-initializesharedaudiostream)):

> "The supported AUDCLNT_STREAMFLAGS_XXX Constants for this parameter when using this method are:
> - AUDCLNT_STREAMFLAGS_EVENTCALLBACK"

One flag. `AUDCLNT_STREAMFLAGS_LOOPBACK` is not in the list, which matches this repo's own
measurement in `docs/recovered/AUDIO_NOTES.md` §3.1:

```
IAudioClient3::InitializeSharedAudioStream(LOOPBACK|EVENTCALLBACK, min_period)
  FAILED: 0x88890021   (AUDCLNT_E_INVALID_STREAM_FLAG)
```

[CONFIRMED — doc + local measurement agree.]

**What periods are reachable.** `GetSharedModeEnginePeriod` returns four frame counts (default,
fundamental, min, max); legal requests are multiples of *fundamental* between *min* and *max*.
Microsoft's own worked example (same page) is a 44.1 kHz endpoint with
`default = 448` (10.16 ms), `fundamental = 4` (0.09 ms), `min = 48` (1.09 ms), `max = 448`. The
in-box HDAudio driver is documented to support **128 samples (2.66 ms at 48 kHz) up to 480 samples
(10 ms)**, and the driver DDI lets a vendor declare a floor as low as 2 ms
(`KSAUDIO_PACKETSIZE_CONSTRAINTS2`, sysvad sample). So 2.67 ms is the realistic small-buffer
target and ~1 ms is the API ceiling.

**But it is per-machine.** On this repo's dev box (Windows 11 26200) the spike measured
`default = fundamental = min = max = 480 frames (10.000 ms)` — the OEM driver exposes *no*
sub-10 ms variant at all. That is why the recommendation below is "query and take what you get",
never "assume 2.67 ms". Note also that switching that machine to the in-box **High Definition
Audio Device** driver is documented to unlock the 128–480 frame range, which is a cheap way to
test the code path.

Failure mode to handle: `AUDCLNT_E_ENGINE_PERIODICITY_LOCKED` — another client already pinned the
engine. Fall back to the default period; do not fail the capture.

**Engine cost on top of the period.** Microsoft puts the Windows 10+ audio-engine processing cost
at **1.3 ms on render and ~0 ms on capture** for all applications, down from ~12/6 ms pre-Win10.
So the period genuinely dominates.

### 2.2 Windows — the idle-endpoint problem, and who else hits it

Microsoft's [Loopback Recording](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)
page says the event problem was fixed long ago:

> "In versions of Windows prior to Windows 10 1703, pull-mode capture client does not receive any
> events when a stream is initialized with event-driven buffering and is loopback-enabled. To work
> around this, initialize a render stream in event-driven mode… In Windows 10 versions 1703 and
> higher, event-driven loopback clients are supported, and no longer need the workaround involving
> the render stream."

**That statement is incomplete, and this repo has the measurement that shows it.**
`AUDIO_NOTES.md` §3.2, on Windows 11 26200:

| Scenario | Event signals | Timeouts | Packets |
|---|---|---|---|
| Host idle, **no** companion | **0 / 20** | 20 | 0 |
| Host idle, **with** silent render companion | **20 / 20** | 0 | 19 (avg 9.39 ms, max 10.29 ms) |

The distinction is that 1703 fixed *event delivery for a loopback stream that has data*; it did
not stop the engine from parking an endpoint with **no render stream anywhere on the system**.
With nothing rendering there is no clock, so there are no buffers and no events. The silent render
companion is still mandatory in 2026.

OBS reaches the same conclusion independently. In
[`plugins/win-wasapi/win-wasapi.cpp`](https://github.com/obsproject/obs-studio/blob/master/plugins/win-wasapi/win-wasapi.cpp)
it initializes loopback with legacy `IAudioClient::Initialize`,
`AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_LOOPBACK`, a 500 ms buffer, an event
handle *and* a 10 ms wait timeout ("Windows 7 does not seem to wake up for LOOPBACK"), and carries
an explicit silence workaround:

> "Silent loopback fix. Prevents audio stream from stopping and messing up timestamps and other
> weird glitches during silence by playing a silent sample."

[CONFIRMED by OBS source.] ScreenExtend's `silence.rs` is the same technique, and its
100 ms event timeout is the same belt-and-braces as OBS's 10 ms one (ours is 10× looser).

### 2.3 Windows — the full tier list

| # | Tier | Availability | Period / added latency | Notes |
|---|---|---|---|---|
| 1 | **Legacy endpoint loopback** — `IAudioClient::Initialize(SHARED, LOOPBACK\|EVENTCALLBACK)` on the `eRender`/`eConsole` endpoint | Vista+ (event-driven reliably 1703+) | one engine period: **10 ms default; 2.67 ms floor on small-buffer drivers**, 1.09 ms API floor | The current tier. Needs the silent render companion. Captures the *whole* system mix across all Terminal Services sessions. |
| 1b | **Same, with the engine period lowered by a companion `IAudioClient3` render stream** | Win10 1607+ *and* a driver that declares a small buffer | **2.67–3 ms** where available; unchanged where not | The recommendation in §5.1. Zero change to the capture stream itself. |
| 2 | **Process loopback** — `ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, …)` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` | **Windows 10 Build 20348+** ([docs](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_activation_params)) — i.e. **not** on retail Windows 10 19041–19045 | Same shared-engine period class; not independently measured **[UNVERIFIED]** | Captures a process tree rather than an endpoint. `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` with our own PID = "everything except us". |
| 3 | **Hardware "Stereo Mix" capture device** | Vendor-dependent, user must enable it | One capture period | Microsoft explicitly discourages it: "Not all audio adapters have loopback devices… the user must identify the loopback device and enable it." Not worth a tier. |
| 4 | **WinRT `AudioGraph`** with `QuantumSizeSelectionMode.LowestLatency` | Win10+ | "the minimum value that is supported by the driver" | Microsoft measures AudioGraph as **strictly worse than WASAPI** — it "adds one buffer of latency in the capture side" and another on render above 6 ms buffers. Also has no loopback mode. Rejected. |
| 5 | **Exclusive mode / ASIO** | — | lowest, but | Exclusive mode *cannot* be combined with loopback ("A client can enable loopback mode only for a shared-mode stream"). Structurally impossible for us. |

**Process loopback, specifics.** Microsoft's own
[`ApplicationLoopback` sample](https://github.com/microsoft/Windows-classic-samples/tree/main/Samples/ApplicationLoopback)
fills the struct exactly as expected and — importantly for the brief — **does use event-driven
mode**:

```c
audioclientActivationParams.ActivationType = AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK;
audioclientActivationParams.ProcessLoopbackParams.ProcessLoopbackMode = includeProcessTree ?
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE : PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE;
audioclientActivationParams.ProcessLoopbackParams.TargetProcessId = processId;
// ...
m_AudioClient->Initialize(AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK |
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, 0, 0, &m_CaptureFormat, nullptr);
```

So the brief's premise "event-driven mode unsupported → must poll" is **wrong for the shipping
sample and for OBS**, both of which use an event handle. What *is* true, and matches OBS's code
shape, is that the event is not a reliable-enough wakeup on its own: both the sample and OBS pair
it with a short wait timeout. OBS uses `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE` and
supplies an **explicitly constructed** 32-bit float `WAVEFORMATEXTENSIBLE` rather than the device
mix format — the virtual loopback device has no mix format of its own to query, which is the real
"format is fixed" constraint. [CONFIRMED by OBS + MS sample.]

The `EXCLUDE_TARGET_PROCESS_TREE`-with-own-PID trick is exactly what the enum is for, and the
sample exposes it as a first-class mode. [CONFIRMED by the sample's ternary above; not
independently run here.] Whether the process-loopback virtual device still needs a silent render
companion is **[UNVERIFIED]** — it is a synthetic device, so the endpoint-parking argument may not
apply, but nobody has published a measurement.

### 2.4 Windows — `AUTOCONVERTPCM`, and a naming trap

The flag descriptions
([AUDCLNT_STREAMFLAGS_XXX](https://learn.microsoft.com/en-us/windows/win32/coreaudio/audclnt-streamflags-xxx-constants)):

- `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` (0x80000000): "A channel matrixer and a sample rate
  converter are inserted as necessary to convert between the uncompressed format supplied to
  IAudioClient::Initialize and the audio engine mix format."
- `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` (0x08000000): "a sample rate converter with **better**
  quality than the default conversion but with a **higher performance cost** is used. This should
  be used if the audio is ultimately intended to be heard by humans."

**The name is backwards from the semantics** and `loopback.rs:26-27` reads as though it selects
"default quality". It actually selects the *expensive, better* converter. That is the right choice
for us (the audio is heard by humans), but the constant deserves a comment. Neither flag has a
documented latency figure; the SRC is a filter with group delay, so it is not free.
**[UNVERIFIED]** — measurable by comparing `capture_ns`-to-speaker on a 48 kHz endpoint vs. a
44.1 kHz one.

The better tool here is `AUDCLNT_STREAMOPTIONS_MATCH_FORMAT` via
`IAudioClient2::SetClientProperties`, documented as the way to "render/capture in the format it
specifies without any resampling by the audio engine" — but it can fail with
`AUDCLNT_E_ENGINE_FORMAT_LOCKED`, and it does not help a genuinely 44.1 kHz endpoint (something
must resample). In practice the endpoint mix format is 48 kHz float32 stereo on every modern
Windows box, and the repo's own measurement on the dev machine confirms that; the AUTOCONVERT path
is the rare case, not the hot one.

### 2.5 Windows — MMCSS and timer resolution

Microsoft's guidance is that WASAPI clients should tag work as "Audio" or "Pro Audio", and that
below a buffer threshold Windows enters a protected mode that isolates the audio engine thread,
the driver's registered interrupts/threads, and "some or all of the audio threads from the
applications that request small buffers, and from all applications that share the same audio
device graph … with any application that requested small buffers."

Two consequences that matter here:

1. **The protection is coupled to the small-buffer request.** A process that never requests small
   buffers gets MMCSS priority but not the glitch-resilience mode. Lowering the engine period
   (§5.1) therefore buys scheduling protection *as well as* latency.
2. Microsoft recommends the **Real-Time Work Queue API** (`RtwqLockSharedWorkQueue(L"Audio", …)`)
   over hand-rolled MMCSS threads. Ours is a hand-rolled `AvSetMmThreadCharacteristicsW("Pro Audio")`
   thread (`guards.rs`), which is what OBS does too and is fine; the RTWQ path is an alternative,
   not an upgrade, for a single dedicated capture thread.

`timeBeginPeriod(1)` / `NtSetTimerResolution` do **not** drive the WASAPI shared-mode event: the
event is signalled by the audio engine's own DPC-driven cadence, not by a user-mode timer. No
primary source states this in so many words **[UNVERIFIED]**, but the mechanism (engine period →
`SetEventHandle` signal) leaves no room for the multimedia timer. Our 100 ms `event.wait(100)`
timeout is a starvation fallback, not a scheduling knob.

### 2.6 macOS — the tier list

| # | Tier | Availability | Buffer / added latency | Notes |
|---|---|---|---|---|
| 1 | **Core Audio Process Tap** — `AudioHardwareCreateProcessTap` + `CATapDescription` + private aggregate device + `AudioDeviceIOProcID` | 14.2+ (functions); the class ships earlier | **one aggregate IO cycle.** Default is the aggregate's inherited buffer frame size (commonly 512 frames = 10.67 ms); settable via `kAudioDevicePropertyBufferFrameSize` | Audio-only, no screen-recording indicator. The tier we already prefer. **We never set the buffer size** — see §5.3. |
| 2 | **ScreenCaptureKit audio** — `SCStreamConfiguration.capturesAudio` | 13.0+ | one `CMSampleBuffer` per delivery; SCK does not document the frame count **[UNVERIFIED]** | Requires a video stream to ride on (we use a 2×2/1 fps dummy), so it lights the screen-recording indicator. |
| 3 | **Self-installed `AudioServerPlugIn` virtual device** set as default output, read back over shared memory | 10.15–12.x (any macOS, but only selected below 13.0) | driver IO cycle + shm ring + reader poll. HAL virtual devices typically negotiate 512 frames = 10.67 ms **[UNVERIFIED]** | The tier that owns the user's default output while streaming. Privacy-relevant, opt-in. |
| 4 | *(nothing else)* | — | — | Pre-13.0 there is genuinely no first-party system-audio API; every product in this space (BlackHole, Loopback, Soundflower, older OBS) ships a driver. |

**`CATapDescription`'s surface**, from Apple's documentation JSON
([catapdescription.json](https://developer.apple.com/tutorials/data/documentation/coreaudio/catapdescription.json)):

initializers `init()`, `init(processes:deviceUID:stream:)`, `init(excludingProcesses:deviceUID:stream:)`,
`init(stereoMixdownOfProcesses:)`, `init(monoMixdownOfProcesses:)`,
`init(stereoGlobalTapButExcludeProcesses:)`, `init(monoGlobalTapButExcludeProcesses:)`;
properties `muteBehavior` (`CATapMuteBehavior`), `isPrivate`, `isMono`, `isExclusive`, `isMixdown`,
`isProcessRestoreEnabled`, `deviceUID`, `stream`, `uuid`, `name`, `processes`, `bundleIDs`.

Apple's JSON reports all of these as "macOS 12.0+", which is **wrong or at least misleading** —
the `AudioHardwareCreateProcessTap` family is `API_AVAILABLE(macos(14.2))` and the class is not
usable without it. This is a good argument for the doctrine ScreenExtend already follows for the
tap functions (`dlsym`) and should follow everywhere: **the documentation's version numbers are
not trustworthy; probe.**

`isProcessRestoreEnabled` and `bundleIDs` are new-ish and not used by our code or by the reference
implementation; `isPrivate = true` (which we set) keeps the tap out of other apps' device lists.

**The silent-tap failure mode is real and well known.** The aggregate device's
`kAudioAggregateDeviceTapListKey` entry must carry a `kAudioSubTapUIDKey` equal to the
`CATapDescription`'s `uuid`, or the tap returns `noErr` and pure silence. Our `process_tap.rs`
builds the dictionary the same way the reference
[AudioCap](https://github.com/insidegui/AudioCap) does (`kAudioAggregateDeviceNameKey`,
`UIDKey`, `MainSubDeviceKey`, `IsPrivateKey`, `IsStackedKey`, `TapAutoStartKey`,
`SubDeviceListKey`, `TapListKey`), and already has a 5 s all-silence watchdog log — that is the
right shape.

**AudioCap does not set `kAudioDevicePropertyBufferFrameSize` either.** [CONFIRMED by reading
`ProcessTap.swift`.] So the reference we copied simply never considered latency; there is headroom
nobody in this space has taken.

For the aggregate's achievable minimum, the best public evidence is Chromium's macOS audio
manager, which queries `kAudioDevicePropertyBufferFrameSizeRange` and deliberately runs **256
frames** on output because "kMinAudioBufferSize is too small for the output side because CoreAudio
can get into under-run if the renderer fails delivering data to the browser within the allowed time
by the OS" ([`audio_manager_mac.cc`](https://github.com/chromium/chromium/blob/main/media/audio/mac/audio_manager_mac.cc)).
Chromium's `limits::kMinAudioBufferSize` is 128 frames. So **128–256 frames (2.7–5.3 ms) is the
range real shipping software uses on macOS**, and a *capture* IOProc has none of the render-side
under-run risk Chromium is guarding against.

**SCK audio config**, from Apple's
[SCStreamConfiguration JSON](https://developer.apple.com/tutorials/data/documentation/screencapturekit/scstreamconfiguration.json):
`capturesAudio`, `sampleRate`, `channelCount`, `excludesCurrentProcessAudio`, `captureMicrophone`,
`microphoneCaptureDeviceID`, plus `minimumFrameInterval`, `queueDepth`. Apple's JSON collapses
every property's availability to the class minimum (12.3), which is again unreliable —
`capturesAudio` and friends are 13.0 and the microphone pair is 15.0. Probe by
`respondsToSelector:`, not by version. There is **no audio-only SCK mode** in any release: the
dummy video stream is unavoidable, which is precisely why the Process Tap outranks it.

Nothing in macOS 15 or 26 supersedes the Process Tap for system audio; the tap *is* the modern
answer and Apple has been extending it (`isProcessRestoreEnabled`, `bundleIDs`) rather than
replacing it. **[UNVERIFIED — no WWDC transcript was reachable in this session; worth one search
when the budget resets.]**

### 2.7 Opus — the FEC question, answered

The single most important correction in this report. opus-codec.org's encoder CTL documentation
([1.5 API](https://opus-codec.org/docs/opus_api-1.5/group__opus__encoderctls.html)):

- `OPUS_SET_INBAND_FEC`: "Configures the encoder's use of inband forward error correction (FEC)."
  Values 0 / 1 / 2. **"This is only applicable to the LPC layer"**.
- `OPUS_SET_DTX`: same note — **"Only applicable to the LPC layer"**.

The LPC layer is SILK. `OPUS_APPLICATION_RESTRICTED_LOWDELAY` "disables the speech-optimized mode"
(i.e. CELT-only), and independently, "Passing in a duration of less than 10 ms (480 samples at
48 kHz) will prevent the encoder from using the LPC or hybrid modes" — our frames are 5 ms.
**So in ScreenExtend's configuration, in-band FEC and DTX are both structurally inert. Turning
either on would change nothing.** [CONFIRMED by opus docs, twice over.]

This confirms the aside already in `findings/major-audio-no-fec-on-an-unreliable-datachannel.md`
and **retires** `findings/speculative-audio-win-dtx-off-so-silence-is-fully-encoded-and-sent.md`'s
one-line alternative ("Enabling `OPUS_SET_DTX_REQUEST, 1` is the one-line alternative") — it is not
an alternative at all. The header-only-keepalive design in that finding's option 2 is the only
workable form.

The corollary is that the SDP contradiction is worse than "a lie": `OPUS_FALLBACK_FMTP =
"minptime=5;useinbandfec=1;…"` (`webrtc_session.rs:42`) advertises a capability that *cannot* be
produced by a `RESTRICTED_LOWDELAY` encoder at 5 ms, on either transport path.

Other Opus facts worth recording:

- Permitted frame sizes at 48 kHz: **120, 240, 480, 960, 1920, 2880** samples. 2.5 ms (120) is
  legal in CELT-only; we use 240 (5 ms).
- `OPUS_GET_LOOKAHEAD` "Gets the total samples of delay added by the entire codec." Measured **120
  samples = 2.5 ms** with our settings (`AUDIO_NOTES.md` build-verification section). Total
  algorithmic delay is therefore frame (5 ms) + lookahead (2.5 ms) = **7.5 ms**, not 5.
- `OPUS_SET_VBR_CONSTRAINT` default is **1 (constrained)**; we explicitly set **0
  (unconstrained)** (`encoder.rs:98`). Unconstrained VBR lets a single frame's size spike well
  above the nominal rate, which on a 200 packet/s DataChannel converts directly into arrival
  jitter. Constrained VBR is what a real-time stream wants; hard CBR is what WebRTC's own Opus
  usage defaults toward for congestion predictability. This is a free change.
- `OPUS_SET_PREDICTION_DISABLED = 1` "disables almost all use of prediction, making frames almost
  completely independent" — the correct dial for a lossy unordered transport where every frame may
  have to stand alone. Costs bitrate at fixed quality.
- `OPUS_SET_COMPLEXITY` 0–10; we use 5. Encode time is already measured into
  `encode_p50_ns`/`encode_p99_ns` and is sub-millisecond, so there is nothing to buy here.
- `OPUS_SET_LSB_DEPTH` default 24; our source is float32 from the mix, so 24 is honest.
- `OPUS_SET_PHASE_INVERSION_DISABLED` is not in the 1.5 CTL page fetched **[UNVERIFIED]**; it
  exists in `opus_defines.h` and only matters for mono-downmix-safety, which is not our problem.

**What to use instead of FEC.** Since both ends are ours and the payload is ours, application-level
redundancy is trivial: append the previous frame's Opus payload to each packet (RFC 2198-style
"RED" without the RTP framing). At 128 kbps / 5 ms the payload measured **118 bytes**
(`AUDIO_NOTES.md`), against ~50–70 bytes of SCTP/DTLS/UDP/IP overhead already being paid per
packet — so duplication roughly doubles payload but far less than doubles wire bytes, and it makes
any *single* packet loss invisible. The existing FEC finding already proposes this; this report
supplies the evidence that it is the *only* option rather than one of two.

### 2.8 The client playback floor

**Browser support has moved, and the repo's assumptions are now out of date.** From MDN's
`browser-compat-data`:

| Feature | Chrome | Firefox | Safari |
|---|---|---|---|
| `AudioDecoder` (WebCodecs) | 94 | **130** | **26** |
| `AudioContext.baseLatency` | 58 | 70 | 14.1 |
| `AudioContext.outputLatency` | **102** | 70 | **18.4** |
| `RTCRtpReceiver.jitterBufferTarget` | 124 | 115 | 27 |

Two consequences:

1. `AUDIO_NOTES.md` §3.4 records "Safari: WebCodecs `AudioDecoder` Opus support is the likely gap".
   **Safari 26 ships `AudioDecoder`.** The client's `detectCapabilities()` (`audio.js:13-21`) tests
   only that the *constructor exists*, so Safari 26 will now select the fast path. Whether Safari's
   decoder accepts `codec: 'opus'` is a separate question the code never asks — and the failure is
   silent (§5.6).
2. `outputLatency` did not exist before Safari 18.4 / Chrome 102. `outputLatencyMs()`
   (`audio.js:418-423`) treats a missing property as **0**, so on those browsers the A/V-sync
   target is short by the entire output latency. On a Bluetooth sink that is 100–200 ms.

**The worklet floor.** The render quantum is 128 frames = **2.67 ms at 48 kHz**. The Web Audio 1.1
spec adds `AudioContextRenderSizeCategory { "default", "hardware" }` and a readonly
`renderQuantumSize`, where "hardware" means "The User-Agent picks a render quantum size that is best
for the current configuration" ([spec](https://www.w3.org/TR/webaudio-1.1/)) — but there is no
numeric override, and it is not in `browser-compat-data` as shipped anywhere. So 128 frames is the
floor and there is nothing to tune. `latencyHint: "interactive"` is already the spec default and is
already what we pass; it is defined as "Provide the lowest audio output latency possible without
glitching."

**Where the client's real cost is.** Not the worklet quantum — it is the commanded jitter depth.
`MIN_TARGET_MS = 10` (`audio.js:6`) floors the buffer at 10 ms = two Opus frames, which is
defensible. The uncontrolled cost is the **re-prime penalty on underrun**: `process()`
(`audio-worklet.js:178-188`) sets `this.priming = true` after any underrun, and priming outputs
digital silence until `avail >= effTarget` again. One lost 5 ms packet therefore costs a full
target depth (10–40 ms) of silence, not 5 ms. That asymmetry is the biggest audible-quality lever
in the client and is not currently anybody's finding (§5.5).

### 2.9 End-to-end budget, best case and today

| Stage | Today (measured/derived) | Floor reachable with §5 |
|---|---|---|
| Capture period (Windows) | **10 ms** (dev box; driver floor) | **2.67 ms** on small-buffer drivers, 10 ms otherwise |
| Accumulator residue (period not a multiple of 5 ms) | 0–5 ms | 0 (2.67 ms period → residue, but bounded by one frame) |
| Opus frame + lookahead | **7.5 ms** (5 + 2.5) | 7.5 ms (5 ms is the right frame size) |
| Opus encode | <1 ms (p50 measured) | unchanged |
| 3 thread hops host-side | ~50–150 µs | ~50 µs (bridge hop removal, existing finding) |
| Network, LAN | 1–5 ms | unchanged |
| Client decode (WebCodecs) | sub-ms **[UNVERIFIED]** | unchanged |
| Client jitter buffer | 10–40 ms adaptive, or A/V-commanded | 10 ms floor |
| `AudioContext` output | 3–10 ms desktop; 20+ ms iOS; **100–200 ms Bluetooth** | platform floor |
| **Total** | **~32–48 ms wired, +100–200 ms on BT** | **~25–35 ms wired** |

The capture period is the largest single controllable term on Windows and the aggregate IO cycle is
the largest on macOS. Everything else is already close to its floor.

---

## 3. How others do it

- **OBS Studio** (`plugins/win-wasapi/win-wasapi.cpp`): legacy `IAudioClient::Initialize` for
  loopback (never `IAudioClient3`), `EVENTCALLBACK | LOOPBACK`, 500 ms buffer, event handle +
  10 ms timeout, and a "silent loopback fix" that plays silence to keep the stream alive. For
  application audio it uses `ActivateAudioInterfaceAsync` with
  `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE` and a hand-built float32
  `WAVEFORMATEXTENSIBLE`. OBS does **not** try to lower the engine period — it is a recorder, not
  a real-time link, so it has no reason to.
- **Microsoft's `ApplicationLoopback` sample**: the canonical process-loopback shape, event-driven,
  `LOOPBACK | EVENTCALLBACK | AUTOCONVERTPCM`, both buffer parameters 0, an explicit 16-bit
  44.1 kHz stereo `WAVEFORMATEX` (i.e. the caller picks the format; the virtual device does not
  have one).
- **AudioCap** (insidegui, the de-facto reference for the macOS Process Tap): builds the aggregate
  dictionary exactly as our `process_tap.rs` does, sets `kAudioAggregateDeviceTapAutoStartKey`,
  and — notably — **never sets a buffer frame size**. Nobody in the Process Tap ecosystem has
  optimised the IO cycle yet.
- **Chromium** (`media/audio/mac/audio_manager_mac.cc`): queries
  `kAudioDevicePropertyBufferFrameSizeRange`, has `limits::kMinAudioBufferSize` = 128 frames, and
  runs 256 frames on output for under-run safety, scaling to 2×/4× above 96/192 kHz. This is the
  best public evidence for what CoreAudio actually accepts.
- **Microsoft's own AudioGraph vs WASAPI measurement**: AudioGraph is one buffer worse on capture
  and another on render above 6 ms. Any WinRT-based tier is a latency regression.

---

## 4. What ScreenExtend does today

Windows (`src-tauri/src/windows_utils/audio/`):

- `loopback.rs:307-322` — legacy `IAudioClient::Initialize(AUDCLNT_SHAREMODE_SHARED, LOOPBACK |
  EVENTCALLBACK, 0, 0, pwfx, None)` on the `eRender`/`eConsole` endpoint; both duration parameters
  are 0, so the engine grants its default period and **the period is never queried**.
- `loopback.rs:355` — `let period_frames = client.GetBufferSize()`, i.e. the total buffer, stored
  in a field named `period_frames` and reported as `period={}fr`.
- `loopback.rs:330-346` — the non-48k path adds `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`
  (constants at `:26-27`).
- `silence.rs:91-100` — the companion also uses **legacy `IAudioClient::Initialize`** with the
  endpoint mix format. This is the stream that could be asking for a small engine period and is not.
- `loopback.rs:197` — `session.event.wait(100)`; `:245` — `capture_ns: host_now_ns()` taken at
  *encode* time, not capture time (existing finding).
- `guards.rs` — `AvSetMmThreadCharacteristicsW("Pro Audio")`.
- `device.rs` — `IMMNotificationClient` posting `DefaultRenderChanged` over crossbeam; never blocks.

Shared core (`src-tauri/src/streamer/audio/`):

- `encoder.rs:72` — `OPUS_APPLICATION_RESTRICTED_LOWDELAY`; `:6-7` — 5 ms / 240 samples;
  `:89-98` — seven CTLs: bitrate 128 kbps, complexity 5, `SIGNAL_MUSIC`, DTX 0, **INBAND_FEC 0**,
  **VBR 1**, **VBR_CONSTRAINT 0**. `OPUS_SET_PACKET_LOSS_PERC` is declared in `opus_sys.rs:37` and
  never issued.
- `encoder.rs:100-102` — `OPUS_GET_LOOKAHEAD`, `.unwrap_or(0)`; propagated as
  `AudioCapture.lookahead_samples` and **only logged** — no consumer.
- `mod.rs:19-33` — the shared `HOST_EPOCH` monotonic clock; `host_ns_to_rtp90k` ties audio
  `capture_ns` and video RTP to one timebase. This part is genuinely right.
- `mod.rs:93` — `BROADCAST_CAPACITY = 128` packets = 640 ms.
- `protocol.rs:3` — 13-byte header (seq u32 LE, capture_ns u64 LE, flags u8) + raw Opus.

macOS (`src-tauri/src/macos_utils/audio/`):

- `mod.rs:106-118` — `capable_native_backends()` gates on `macos_at_least(14, 2)` **and**
  `process_tap::runtime_available()`; the version check is redundant with the probe and violates
  the tier doctrine (existing finding).
- `process_tap.rs:40-59` — `dlsym("AudioHardwareCreateProcessTap")` +
  `AnyClass::get(c"CATapDescription")`. Correct dyld-safe probing.
- `process_tap.rs:281` — `initStereoGlobalTapButExcludeProcesses:` with an empty array = global tap.
- `process_tap.rs:320` — `set_nominal_sample_rate(output_dev, 48_000.0)` on the **user's** device
  (existing finding).
- `process_tap.rs:700-757` — the aggregate dictionary: Name, UID, MainSubDevice, IsPrivate,
  IsStacked, TapAutoStart, SubDeviceList, TapList (+ `kAudioSubTapUIDKey`,
  `kAudioSubTapDriftCompensationKey`). **`kAudioDevicePropertyBufferFrameSize` appears nowhere in
  the file.**
- `mod.rs:23` — `RING_CAPACITY` = 80 ms; `mod.rs:349` — `park_timeout(125 µs)` drain loop.

Client (`src-tauri/src/streamer/static/`):

- `audio.js:13-21` — capability detection: constructor existence only, no
  `AudioDecoder.isConfigSupported`.
- `audio.js:471` — `new AC({ latencyHint: 'interactive', sampleRate: SR })`.
- `audio.js:418-423` — `outputLatencyMs()` = `(baseLatency ?? 0) + (outputLatency ?? 0)`.
- `audio.js:6-7` — target clamped to `[10, 400]` ms.
- `audio-worker.js:114` — `decoder.configure({codec:'opus', sampleRate:48000, numberOfChannels:2})`,
  no `description` (correct for raw Opus per the WebCodecs codec registry).
- `audio-worker.js:126-129` — dedup + late-drop by `seq`; **no gap detection, no PLC**.
- `audio-worklet.js:178-188` — underrun ⇒ `priming = true` ⇒ silence until the target refills.

---

## 5. Recommendations

Ranked by expected latency/quality delta per unit of implementation cost.

### 5.1 Lower the endpoint's engine period from the silent render companion (Windows)

**The single biggest win available, and it is documented behaviour.**

- **API**: on the companion's `IAudioClient`, `QueryInterface` for `IAudioClient3`; call
  `GetSharedModeEnginePeriod(mixformat, &default, &fundamental, &min, &max)`; if `min < default`,
  initialize the companion with `InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
  min, mixformat, nullptr)` instead of the current legacy `Initialize`. The capture stream is
  **unchanged** — it keeps its legacy loopback `Initialize` and simply starts being clocked faster.
- **Platform**: Windows 10 1607+ for `IAudioClient3`; the benefit needs a driver that declares a
  small buffer.
- **Probe**: `QueryInterface(IID_IAudioClient3)` succeeding, then `min < default`. Never a version
  check. Handle `AUDCLNT_E_ENGINE_PERIODICITY_LOCKED` and `AUDCLNT_E_INVALID_DEVICE_PERIOD` by
  falling back to the current legacy path.
- **Win**: **−5 to −7.3 ms** of capture latency on small-buffer hardware (10 ms → 2.67 ms), plus
  Windows' small-buffer glitch-protection mode for our audio threads. **0 ms** on hardware like the
  current dev box, where `min == default == 480`. Order the companion *before* the capture stream
  (as `acquire()` already does) so the engine is already at the small period when loopback opens.
- **Measure**: log all four `GetSharedModeEnginePeriod` values and the granted period; then the
  click-track procedure in `AUDIO_NOTES.md`, and the packet-interval histogram the spike already
  produces (expect avg ~2.7 ms instead of ~9.4 ms).

Finding: `findings/major-audio-win-engine-period-lowerable-via-render-companion.md`

### 5.2 Set the tap aggregate device's IO buffer size (macOS)

- **API**: after `AudioHardwareCreateAggregateDevice` and before `AudioDeviceStart`, read
  `kAudioDevicePropertyBufferFrameSizeRange` on the aggregate and set
  `kAudioDevicePropertyBufferFrameSize` to `max(range.mMinimum, 128)`.
- **Platform**: every macOS with a Process Tap (and harmless on any aggregate).
- **Probe**: the property's own `AudioObjectSetPropertyData` return code — accept whatever the HAL
  grants, re-read to confirm.
- **Win**: **−5 to −8 ms** if the default is 512 frames (10.67 ms) and 128–256 is accepted.
  Chromium runs 256 frames on *output*; a capture IOProc has strictly less under-run exposure.
- **Measure**: log the granted frame size and the observed IOProc callback interval; watch
  `consumer.overruns()` and the ring's fill for regressions.

Finding: `findings/major-audio-mac-tap-aggregate-buffer-frame-size-never-set.md`

### 5.3 Stop losing a whole jitter buffer to one lost packet (client)

- **Change**: in `audio-worklet.js`, on underrun, do **not** set `priming = true`. Emit the
  concealment (a fade to silence over the short block, or a repeat of the last block) and keep
  reading. Re-prime only after N consecutive underrun blocks (i.e. a genuine stall), not after one.
  In parallel, `audio-worker.js` should detect a `seq` gap and synthesize the missing frames as
  silence so the timeline stays continuous.
- **Platform**: all.
- **Win**: converts a single 5 ms loss from **10–40 ms of silence** into 5 ms of concealed audio.
  No latency cost. This is the largest audible-quality lever in the client.
- **Measure**: induce 1 %/2 %/5 % loss (`clumsy`, `netem`); count `underruns` and `corrections`
  from `SEAudio.getSyncInfo()` and A/B listen.

Finding: `findings/major-client-one-lost-packet-costs-a-full-jitter-buffer.md`

### 5.4 Give `outputLatency` a fallback so A/V sync is not silently short (client)

- **Change**: when `AudioContext.outputLatency` is `undefined` (Safari < 18.4, Chrome < 102,
  any WebView on the 10.15 floor), do not treat it as 0. Estimate it — start from `baseLatency`,
  and refine with an `AudioContext.getOutputTimestamp()`-based estimate
  (`contextTime - performance.now()/1000` drift) where available. Surface the fact that it is an
  estimate in `getSyncInfo()`.
- **Platform**: all; matters most on iOS Safari and on any Bluetooth sink.
- **Win**: removes a systematic A/V-sync error equal to the true output latency — tens of ms on a
  wired sink, **100–200 ms on Bluetooth**, where audio currently plays that far *ahead* of picture.
- **Measure**: film the host click and the client screen together; compare
  `residualOffsetMs` against the photographed offset, wired and over Bluetooth.

Finding: `findings/major-client-outputlatency-absent-breaks-avsync-on-safari-and-bluetooth.md`

### 5.5 Constrain VBR and disable prediction (shared Opus encoder)

- **API**: `OPUS_SET_VBR_CONSTRAINT = 1` (currently explicitly 0); consider
  `OPUS_SET_PREDICTION_DISABLED = 1` on the DataChannel path.
- **Platform**: all.
- **Win**: no latency change, but bounds peak packet size, which directly reduces arrival jitter
  and therefore the jitter-buffer depth the client has to command. Prediction-disabled makes each
  frame independent, which is what a zero-retransmit transport needs. Costs some bitrate at fixed
  quality — 128 kbps stereo has room.
- **Measure**: packet-size histogram on the DataChannel before/after; commanded `targetMs` from
  `getSyncInfo()` over a 10-minute session.

Finding: `findings/minor-audio-opus-unconstrained-vbr-inflates-jitter.md`

### 5.6 Verify the decoder actually supports Opus before committing to the fast path (client)

Newly urgent: **Safari 26 ships `AudioDecoder`**, so `detectCapabilities()` now returns
`webcodecsOpus: true` on Safari, and if Safari's decoder rejects `codec: 'opus'` the current code
produces **permanent silence with no fallback** — `makeDecoder()`'s error handler recreates the
decoder in a loop and the page is never told.

- **Change**: `await AudioDecoder.isConfigSupported({codec:'opus', sampleRate:48000,
  numberOfChannels:2})` in `detectCapabilities()`; and have `audio-worker.js` post a
  `{type:'decoder-failed'}` message so `audio.js` can tear down and fall back to the NetEQ track.
- **Platform**: all; specifically de-risks Safari 26+ and Firefox 130+, both of which have just
  entered the fast path.
- **Win**: not latency — availability. Prevents a silent total-audio failure on two browser
  families that were previously excluded by the constructor check.
- **Measure**: force `isConfigSupported` to false and confirm the NetEQ path engages.

Finding: `findings/major-client-audiodecoder-opus-support-never-verified.md`

### 5.7 Add a probed process-loopback tier (Windows) — lower priority

- **API**: `ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, IID_IAudioClient,
  &AUDIOCLIENT_ACTIVATION_PARAMS{ PROCESS_LOOPBACK, {EXCLUDE_TARGET_PROCESS_TREE, GetCurrentProcessId()} }, …)`,
  then `Initialize(SHARED, LOOPBACK|EVENTCALLBACK|AUTOCONVERTPCM, 0, 0, our_own_48k_f32_stereo_wfx, nullptr)`.
- **Platform**: Windows 10 Build 20348+ — **not** retail Windows 10 19041–19045, so it must be a
  probed tier, not a replacement.
- **Probe**: `GetProcAddress(LoadLibraryW(L"Mmdevapi.dll"), "ActivateAudioInterfaceAsync")` plus
  a successful activation. Never `GetVersionEx`.
- **Win**: **not primarily latency.** It buys (a) capture that is independent of the default
  endpoint, so a device change or an endpoint held in exclusive mode does not kill audio, (b) a
  format we choose rather than the endpoint's, removing the `AUTOCONVERTPCM` SRC on 44.1 kHz
  endpoints, and (c) structural exclusion of our own process. Whether it still needs the silent
  companion is **[UNVERIFIED]** and is the first thing to measure.
- **Measure**: the same 20-iteration event/timeout table as `AUDIO_NOTES.md` §3.2, with and without
  the companion, plus the packet-interval histogram.

Finding: `findings/minor-audio-win-process-loopback-tier-not-implemented.md`

### 5.8 Not recommended

- **`AUDCLNT_STREAMFLAGS_LOOPBACK` on `IAudioClient3`** — impossible; the supported-flag list has
  one entry and this is not it.
- **AudioGraph / WinRT loopback** — Microsoft's own measurement puts AudioGraph one full buffer
  behind WASAPI on capture, and it has no loopback mode.
- **Exclusive mode** — mutually exclusive with loopback by documentation.
- **2.5 ms Opus frames** — saves 2.5 ms of the 7.5 ms codec delay but doubles the packet rate to
  400/s, and per-packet SCTP/DTLS/UDP overhead already exceeds the 118-byte payload. Bad trade.
- **Turning on `OPUS_SET_INBAND_FEC` or `OPUS_SET_DTX`** — both are LPC-layer-only and inert in
  our configuration. Use packet-level redundancy and header-only silence keepalives instead.

---

## 6. Verification plan

**Measurable on the Windows dev box (Win 11 26200, RTX 3080 Ti):**

1. Extend `examples/audio_spike.rs` to log all four `GetSharedModeEnginePeriod` values, then run it
   again after switching the endpoint to the in-box **High Definition Audio Device** driver (which
   is documented to expose 128–480 frames). That is the only way to exercise §5.1's happy path on
   this hardware.
2. Packet-interval histogram at the capture thread, before and after §5.1: expect the mean to move
   from ~9.4 ms to ~2.7 ms where the driver allows it.
3. `GetDevicePeriod` vs `GetBufferSize` naming fix, verified by the two log lines agreeing.
4. Loss injection (`clumsy`) at 1/2/5 % with `getSyncInfo()` counters, for §5.3 and §5.5.
5. Browser matrix for §5.4/§5.6: Chrome (fast path + SAB), Firefox 130+ (fast path, new), Safari 26
   if reachable, plus one iOS device and one Bluetooth sink.
6. The click-track end-to-end procedure in `AUDIO_NOTES.md` — still never actually run. It needs
   two-device acoustic capture, not the dev box alone.

**Not measurable on the dev boxes:**

- Everything in §5.2 and the macOS tiers. The Mac is 10.15.8 Intel: `probe_audio_backend()` returns
  `Unsupported`/`NeedsDriverInstall`, there is no Process Tap, no SCK, and no signed driver. The
  aggregate buffer-size change can be written and type-checked but **cannot be run**.
- §5.7's process loopback needs a build ≥ 20348 host (the dev box qualifies) *and* a retail
  Windows 10 19045 host to confirm the probe correctly refuses it. The latter is missing.
- Bluetooth `outputLatency` reporting differs per platform and needs a real BT sink on each of
  Chrome/Android, Safari/iOS and Chrome/Windows.

---

## 7. Open questions

1. **Does lowering the engine period actually change the loopback capture cadence?** Microsoft's
   FAQ says all streams on the endpoint switch to the small buffer. It does not say the *loopback*
   capture buffer follows. This is the one assumption §5.1 rests on and it must be measured before
   the change is called a win. The measurement is cheap (packet-interval histogram) and the code is
   small.
2. **What buffer frame size does a tap aggregate default to, and what minimum will it accept?**
   Neither Apple nor AudioCap documents it. Needs a 14.2+ Mac.
3. **Does process loopback need the silent render companion?** No published measurement exists.
4. **Does Safari 26's `AudioDecoder` support `opus`?** Not in `browser-compat-data`. §5.6 makes the
   answer not matter for correctness, but it decides whether Safari gets the fast path.
5. **What does `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY` cost in latency?** No documented figure. Only
   reachable on a non-48 kHz endpoint — a USB DAC pinned to 44.1 kHz would settle it.
6. **Is there anything in macOS 15/26 newer than the Process Tap?** The `WebSearch` budget ran out
   before WWDC transcripts could be checked. Apple's continued extension of `CATapDescription`
   (`isProcessRestoreEnabled`, `bundleIDs`) suggests no, but this is the one section of the brief
   that is not properly closed.
7. **Does `Instant`-based `host_now_ns()` stay coherent across a Windows sleep/resume?** The audio
   `capture_ns` and video RTP both ride it; a suspend that pauses the monotonic clock differently
   from the audio engine would show up as a permanent A/V offset after resume. Untested.
