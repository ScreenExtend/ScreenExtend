# PRD — System Audio Capture & Streaming

**Repo:** `ScreenExtend/ScreenExtend` (you are running inside it)
**Feature:** Per-device system audio toggle. Captures Windows system output and streams it to the
client browser alongside the existing video.
**Scope:** Windows only. macOS and Linux get compiling stubs that report unsupported.
**Baseline:** Windows Client 20H1 (build 19041, May 2020) / Windows Server 20H2 (build 19042,
October 2020). **Nothing may require a higher build.**

---

## Step 0 — Run the bootstrap

**Before writing any code, before reading any repo file, run this:**

```sh
./generate-sources.sh
```

It clones upstream reference implementations and fetches vendor documentation into `.sources/`
(gitignored, not a build dependency). Then read `.sources/README.md`, which orders the material.

This is not optional and it is not a formality. This feature touches four API surfaces where
recalled knowledge is unreliable and quietly wrong: WASAPI loopback's undocumented behaviors, the
exact numeric values of libopus control macros, `webrtc-rs` 0.17's specific API shape, and what
Chrome's `AudioDecoder` actually accepts for Opus. **Read the real headers and the real
implementations.** OBS's `plugins/win-wasapi/win-wasapi.c` in particular will save you a week of
discovering loopback's failure modes one crash at a time.

If a fetch fails, `.MISSING.txt` files record the URL — read those online rather than proceeding
on memory.

---

## 1. Context: why the hard path

Read `CLAUDE.md` and `README.md` first for the existing architecture.

This codebase has a consistent philosophy, and you must match it. For video, the easy path existed
and was rejected: `gdigrab` piped to FFmpeg, delivered to a `<video>` element, would have worked in
an afternoon. Instead the repo does zero-copy D3D11 capture → NVENC/QSV → WebRTC → **custom
WebCodecs decoding on the client**, bypassing the browser's video jitter buffer entirely. That
bypass is the whole reason the product feels like a monitor rather than a stream.

**Audio must clear the same bar.** The naive implementation — add an Opus track to the existing
`RTCPeerConnection`, let the browser play it — is a two-hour job that hands 40–100 ms of NetEQ
jitter buffer straight back. That would make audio the slowest component in a pipeline
specifically engineered around latency, and it would break lip-sync against a video path that has
no comparable buffer.

So: same shape as video. Capture at the lowest layer available, encode with the lowest-delay
settings the codec offers, transport out-of-band, and decode on the client with WebCodecs into a
buffer whose depth *we* control.

**Latency budget — target end-to-end capture-to-speaker ≤ 35 ms:**

| Stage | Target | Notes |
| --- | --- | --- |
| WASAPI loopback period | 3–10 ms | Device minimum via `IAudioClient3`, else smallest workable |
| Opus encode (CELT-only) | ~8 ms | 5 ms frame + ~2.5–6.5 ms algorithmic lookahead |
| Network (LAN) | 1–5 ms | Same DTLS transport as video |
| Client jitter buffer | 10–15 ms | **Ours**, tunable — not NetEQ's |
| AudioContext output | 3–10 ms | `latencyHint: 'interactive'` |

Treat that table as the acceptance target, and instrument against it (§9).

---

## 2. Explicitly rejected approaches

Do not implement any of these. If you conclude one is necessary, **stop and explain** rather than
silently substituting it.

| Rejected | Why |
| --- | --- |
| FFmpeg subprocess (`dshow`, `wasapi`) | Process boundary, pipe buffering, no control over period size. Adds 100 ms+ and a shipped binary. |
| Virtual audio cable driver (VB-Cable, VAC) | Second driver install on top of the display driver. Routes audio through the engine twice. User's default device gets hijacked. |
| `getDisplayMedia({audio:true})` client-side | Captures the *client's* audio, not the host's. Solves nothing. |
| Naive `IAudioClient::Initialize` + `Sleep()` polling loop | The tutorial pattern. 10 ms default period, no MMCSS, jitter from the scheduler. |
| Opus track on the existing `PeerConnection`, browser playback | The core rejection. NetEQ adds 40–100 ms and is not tunable from JS. **Ships only as the compatibility fallback (§6.3).** |
| Process loopback (`AUDCLNT_ACTIVATION_PARAMS` / `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`) | Requires build 20348+. Violates the stated baseline. Revisit later as a runtime-gated enhancement for per-app audio — leave a comment noting where it would slot in. |
| AAC / AAC-LD / MP3 | AAC-LD isn't in browsers. MP3's delay is worse than Opus hybrid. Opus is the only low-delay codec with universal browser decode. |
| Exclusive-mode WASAPI | Would take exclusive ownership of the user's output device — they'd lose all other audio. Non-starter for a screen-extension tool. |
| Userspace resampling when the mix format is already 48 kHz | Pure added latency and CPU. Handle non-48k, but never resample needlessly (§4.4). |
| A crate that wraps WASAPI (`cpal`, `wasapi-rs`) as a dependency | Read them, don't depend on them. They generalize across backends and hide the period-size control we need. The repo hand-writes its FFI layers (`x264_sys.rs`, `nvenc_sys/`, `intel_sys.rs`) — follow that. |

---

## 3. Spike first — verify these before committing to a design

**Several load-bearing assumptions below are uncertain, and the documentation is thin or
contradictory.** Spend the first work session writing a throwaway `examples/audio_spike.rs` that
answers these empirically on real hardware. Record the answers in `AUDIO_NOTES.md`. Do not build
the real module until you have them.

1. **Does `AUDCLNT_STREAMFLAGS_LOOPBACK` work with `IAudioClient3::InitializeSharedAudioStream`?**
   `IAudioClient3` low-latency shared mode is the fastest documented path, but the supported-flags
   list for `InitializeSharedAudioStream` is a subset and loopback's inclusion is not clearly
   documented. If it fails, fall back to `IAudioClient::Initialize` with the smallest
   `hnsBufferDuration` the device accepts. **Test both; record actual measured period.**

2. **Does event-driven loopback (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`) actually signal?** The
   long-standing behavior is that loopback capture streams do *not* raise the event reliably unless
   an active render stream exists on the same endpoint. Determine what's true on the 19041
   baseline. If events don't fire, you need the silent-render companion stream (§4.3) not just for
   idle-silence but as the clock source.

3. **What is `GetSharedModeEnginePeriod`'s minimum on typical hardware?** Report default,
   fundamental, min, and max for a few devices. This sets the real floor on capture latency.

4. **Does Chrome's `AudioDecoder` accept raw Opus packets without an `OpusHead` description?**
   Configure with `{codec:'opus', sampleRate:48000, numberOfChannels:2}` and feed a raw packet. If
   it requires `description`, you must synthesize an `OpusHead` header. Test Chrome, Edge, and
   Safari — Safari's WebCodecs Opus support is the most likely gap.

5. **What is the smallest Opus frame size the client path handles cleanly?** 2.5 ms is
   theoretically lowest, but per-packet overhead over SCTP and per-callback cost in the worklet may
   make 5 ms or 10 ms net faster. **Measure; don't assume smaller is better.**

6. **Is `SharedArrayBuffer` available?** It requires COOP/COEP headers **and** a secure context.
   Confirm the consequences in §6.4 — this couples the feature to HTTPS.

---

## 4. Host capture — `src-tauri/src/windows_utils/audio/`

New module. Mirror the layout and conventions of `windows_utils/streamer/`.

```
src-tauri/src/windows_utils/audio/
├── mod.rs           # public surface: AudioCapture::start() -> (Receiver<AudioPacket>, StopFn)
├── loopback.rs      # WASAPI loopback client, MMCSS thread, event loop
├── silence.rs       # silent render companion stream (keeps the endpoint alive)
├── device.rs        # IMMDeviceEnumerator, default-device tracking, IMMNotificationClient
├── format.rs        # WAVEFORMATEXTENSIBLE negotiation, channel/sample-format conversion
├── opus_sys.rs      # hand-written libopus FFI (mirrors x264_sys.rs)
├── encoder.rs       # OpusEncoder wrapper: config, encode, RAII cleanup
└── test/
    ├── mod.rs
    ├── format.rs    # format conversion + downmix unit tests
    └── opus_layout.rs  # FFI struct/constant verification (mirrors nvenc_layout.rs)
```

### 4.1 Loopback client

- `IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)` — capture the **render**
  endpoint with the loopback flag. Console role, not multimedia.
- Initialize per the §3.1 spike outcome: `IAudioClient3::InitializeSharedAudioStream` at minimum
  periodicity if loopback is supported there, else `IAudioClient::Initialize` with
  `AUDCLNT_SHAREMODE_SHARED | AUDCLNT_STREAMFLAGS_LOOPBACK` and the smallest accepted buffer.
- **COM apartment:** initialize MTA (`COINIT_MULTITHREADED`) on the capture thread. Do not touch
  the Tauri main thread's apartment.
- **MMCSS is mandatory.** `AvSetMmThreadCharacteristicsW(L"Pro Audio", &task_index)` on the capture
  thread, `AvRevertMmThreadCharacteristics` on teardown. Without it the scheduler will give you
  periodic 10 ms+ stalls and the whole latency budget is gone. Wrap the handle in an RAII guard so
  it reverts on early return.
- Loop: wait on the event handle (or the companion stream's event) →
  `IAudioCaptureClient::GetBuffer` → handle flags → encode → send → `ReleaseBuffer`. Never allocate
  in this loop; use a preallocated scratch buffer.

### 4.2 Buffer flags — handle all of them

- `AUDCLNT_BUFFERFLAGS_SILENT` — the data pointer's contents are **undefined**, not zeroed. Emit
  silence explicitly; do not encode whatever bytes happen to be there.
- `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY` — a glitch occurred. Log it, count it, expose the
  counter in diagnostics (§9), and reset the encoder's expectations rather than pretending the
  timeline is continuous.
- `AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR` — the QPC position is unreliable for this packet; fall back
  to the extrapolated timeline.
- `AUDCLNT_E_DEVICE_INVALIDATED` from any call — the device went away. Tear down and re-acquire the
  new default (§4.5). See how `win-wasapi.c` structures this.

### 4.3 The idle-endpoint problem

**This will bite you and it is not obvious.** When nothing on the host is playing audio, the
Windows audio engine may stop running the endpoint entirely, and loopback capture then delivers
**no packets at all** — not silence, nothing. Your stream stalls, RTP/packet timestamps drift, and
the client's buffer starves. When audio resumes, you get a discontinuity.

Standard fix: open a **second, silent render stream** on the same endpoint (`silence.rs`) that
continuously writes zeroed buffers. This keeps the engine active so loopback produces continuous
silence packets, and — per the §3.2 spike — may also be what makes the event handle signal.

Requirements:
- Minimum possible buffer, same period as the capture stream.
- Write actual zeroed frames, or use `AUDCLNT_BUFFERFLAGS_SILENT` on render.
- It must be inaudible and must not appear as an active session that alters the user's volume mixer
  behavior or prevents system sleep. Verify in the Volume Mixer and with `powercfg /requests`.
- Start it lazily — only while at least one device has audio enabled — and stop it when the last
  one disconnects. Do not keep the host's audio engine pinned awake for a feature nobody is using.

### 4.4 Format handling

The shared-mode mix format is whatever the user's endpoint is set to — commonly 48 kHz float32
stereo, but 44.1 kHz and 24-bit and 5.1/7.1 all occur.

- Call `GetMixFormat`, parse as `WAVEFORMATEXTENSIBLE`, branch on the subformat GUID
  (`IEEE_FLOAT` vs `PCM`) and bit depth.
- **48 kHz is the fast path** — Opus is natively 48 kHz. Pass float32 straight through with zero
  conversion. Optimize for this case.
- **Non-48 kHz:** you need a rate conversion. Two options, in order of preference:
  1. Request `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` and
     ask for 48 kHz float directly. Lets the engine convert. Costs some latency; costs you no
     resampler.
  2. Hand-written resampler if (1) proves too slow or unavailable with loopback.
  **Measure both.** Document the choice in `AUDIO_NOTES.md`.
- **>2 channels:** downmix to stereo. Use the standard ITU coefficients (−3 dB center and
  surrounds); do not naively sum, which clips. Unit-test the matrix in `test/format.rs`.
- **Integer PCM:** convert to float32 for Opus. Watch the asymmetric int16 range — divide by 32768,
  not 32767, and clamp.

### 4.5 Device changes

Implement `IMMNotificationClient` (`device.rs`) and handle `OnDefaultDeviceChanged` for
`eRender`/`eConsole`. When the user switches output — plugs in headphones, changes to HDMI — the
capture must follow within a few hundred ms without dropping the WebRTC connection.

Also handle `OnDeviceStateChanged` (unplugged) and the format changing under you. On any of these:
tear down cleanly, re-acquire, re-negotiate format, log the transition, and continue. **Do not
panic.** Emit silence during the gap so the client's timeline stays continuous.

Cross-check against `CLAUDE.md`'s locking rules: the notification callback fires on a COM thread
you don't own. Do not block it, do not take a lock the capture thread holds. Post a message to the
capture thread over the existing `crossbeam-channel` dependency instead.

---

## 5. Encoding — libopus

### 5.1 FFI shim, repo-style

The repo hand-writes its `*_sys` layers and bundles the DLL — see `x264_sys.rs` +
`resources/libx264-164.dll`, loaded via `libloading` (already a dependency). Follow that exactly:

- `opus_sys.rs` — hand-written FFI. **Read `.sources/repos/opus/include/opus.h` and
  `opus_defines.h` for the real macro values.** `OPUS_SET_BITRATE_REQUEST` and friends are numeric
  constants; getting one wrong gives you a silently misconfigured encoder, not a compile error.
- Bundle `libopus.dll` in `src-tauri/resources/`, add it to `bundle.resources` in
  `tauri.conf.json`, and load it with `libloading` the way x264 is loaded.
- **Add provenance.** Per the repo's outstanding hygiene work, create/extend
  `src-tauri/resources/PROVENANCE.md` with libopus version, source URL, SHA-256, and license
  (BSD-3-Clause — compatible with AGPL-3.0, but state it).
- `test/opus_layout.rs` — verify every constant and struct offset against the header, in the spirit
  of `nvenc_layout.rs`. This is the test that catches a DLL upgrade breaking your assumptions.

### 5.2 Encoder configuration — lowest delay

```
opus_encoder_create(48000, channels, OPUS_APPLICATION_RESTRICTED_LOWDELAY)
```

`RESTRICTED_LOWDELAY` is the entire point: it disables the SILK layer and forces CELT-only,
dropping algorithmic delay from ~26.5 ms to roughly 6.5 ms at 48 kHz. Do not use
`OPUS_APPLICATION_AUDIO` — it sounds marginally better and costs you a fifth of the latency budget.

Then:
- `OPUS_SET_BITRATE` — 128 kbps stereo default. Make it follow the existing per-device video
  quality knob's spirit, but do not add a UI control for it in v1.
- `OPUS_SET_COMPLEXITY` — start at 5, not 10. Higher complexity costs encode time for quality you
  won't hear on system audio. Measure encode time per frame; keep it well under the frame period.
- `OPUS_SET_SIGNAL(OPUS_SIGNAL_MUSIC)` — system audio is predominantly music/effects, not voice.
- `OPUS_SET_DTX(0)` — discontinuous transmission saves bandwidth by dropping silence, but creates
  exactly the timeline gaps we fought to eliminate in §4.3. Off.
- `OPUS_SET_INBAND_FEC(0)` initially. FEC helps on lossy links without adding delay, but it only
  works with `OPUS_SET_PACKET_LOSS_PERC` and a decoder that uses it — and our custom client decoder
  won't unless you implement it. Revisit after §9 shows real loss on Wi-Fi.
- `OPUS_SET_VBR(1)`, `OPUS_SET_VBR_CONSTRAINT(0)` — unconstrained VBR for lowest latency.
- Frame size: per the §3.5 spike. Default to **5 ms** (240 samples @ 48 kHz) unless measurement
  says otherwise.

Wrap the encoder handle in an RAII type so `opus_encoder_destroy` cannot be skipped on an early
return or `?`. Add `// SAFETY:` comments on every `unsafe` block — this is new code and there's no
excuse for it to arrive without them.

---

## 6. Transport and client

### 6.1 Primary path — Opus over DataChannel + WebCodecs

Mirroring the video architecture's jitter-buffer bypass.

**Host** (`src-tauri/src/streamer/webrtc_session.rs`, in `handle_whep_offer` around line 263):

- Create a dedicated DataChannel, label `"audio"`, configured **unordered** with
  **`maxRetransmits: 0`**. Late audio is worse than missing audio; never retransmit.
- Note the existing `on_data_channel` handler at line 295 accepts labels `"fast" | "reliable" |
  "bulk"` for remote input. Audio is host→client, so the host **creates** this channel rather than
  accepting it. Keep it separate from the input channels and do not disturb their handling.
- Wire frames from the audio capture channel into it. Each message: a compact binary header
  (sequence number `u32`, capture QPC timestamp `u64`, flags `u8`) followed by the raw Opus packet.
  Define this in a small `audio/protocol.rs` following the style of
  `src-tauri/src/streamer/input/protocol.rs` — that file is a good model for a tight binary format.
- Backpressure: check `buffered_amount()` before sending. If the channel is congested, **drop the
  oldest packets** rather than queueing. Log the drop count.

**Client** (`src-tauri/src/streamer/static/`):

- New `audio.js` served by a new route in `server.rs` alongside the existing `transform_worker` /
  `input_js` handlers (lines 271–284). Follow that exact pattern — `include_str!`, correct
  `Content-Type`.
- New `audio-worklet.js`, served the same way.
- Decode with `AudioDecoder` (WebCodecs) → `AudioData` → ring buffer → `AudioWorkletProcessor` →
  `AudioContext` destination.
- `new AudioContext({ latencyHint: 'interactive', sampleRate: 48000 })` — matching the source rate
  avoids a browser-side resample.
- The `AudioContext` requires a user gesture to start. The join flow already has one (the OTP
  submit button) — resume the context there. Handle the case where it's suspended anyway.

### 6.2 Jitter buffer — ours, and small

Target depth 10–15 ms (2–3 packets at 5 ms). Implement adaptively:
- Track packet arrival interval variance.
- Grow the buffer on repeated underruns, shrink it after a stable period.
- **Underrun:** emit silence, don't stall. Count it.
- **Overrun:** drop the oldest, don't grow unbounded. Count it.
- Expose current depth and both counters to the diagnostics overlay (§9).

Reference `.sources/repos/ringbuf.js` for the lock-free SAB pattern.

### 6.3 Fallback path — standard WebRTC audio track

Mirror the existing video fallback pattern (the repo already falls back to a transform worker when
WebCodecs is unavailable). If the client lacks `AudioDecoder`, `AudioWorklet`, or a usable
`SharedArrayBuffer`:

- Add a standard Opus `TrackLocalStaticSample` to the peer connection
  (`MIME_TYPE_OPUS`, clock rate 48000, channels 2).
- Set fmtp sensibly: `minptime=10;useinbandfec=0;stereo=1;sprop-stereo=1`.
- Browser handles decode and playback via NetEQ. Higher latency, but it works everywhere.
- **Report which path is active** in the client UI and in host logs. A silent downgrade to the slow
  path would make latency regressions impossible to diagnose.

Detect capability client-side and declare it in the join request (§7.3) so the host knows which to
set up. Do not set up both.

### 6.4 SharedArrayBuffer requires HTTPS — a hard dependency

`SharedArrayBuffer` needs cross-origin isolation (`Cross-Origin-Opener-Policy: same-origin` +
`Cross-Origin-Embedder-Policy: require-corp`) **and** a secure context. The join flow currently
defaults to plaintext `http://` (see `src/lib/utils.ts:63`), which is not a secure context off
localhost.

**Therefore the fast audio path cannot work on the default join URL as it stands today.**

Options, in preference order:
1. **Land the HTTPS-first join migration first.** It is already a queued fix and this feature
   depends on it. Doing it first is cleanest.
2. Implement a non-SAB ring buffer — `postMessage` transferring `Float32Array`s into the worklet.
   Workable, more GC pressure, slightly higher and less predictable latency. Acceptable v1 if
   HTTPS isn't ready.

Pick one, state which, and note the consequence in the client UI if the fast path is unavailable
for this reason. Adding the COOP/COEP headers on the axum responses is also required for option 1 —
verify they don't break the existing video path's worker loading.

### 6.5 A/V sync

Both paths bypass their respective jitter buffers, so neither gets sync for free.

- The video pipeline timestamps frames with `Instant` (`frame.capture`, used in
  `webrtc_session.rs`). Audio gets QPC positions from `GetBuffer`.
- Establish **one** host timebase. Convert both to a common monotonic reference at capture time and
  send it in-band (the audio header already carries it; add it to the video path if it isn't
  already there).
- Client: align playback against the video presentation time. Correct drift by **resampling or
  dropping/duplicating at silence boundaries**, never by hard-cutting mid-tone — that's audible.
- Target: audio within ±20 ms of video. Beyond ~40 ms, lip-sync is perceptible.
- Expose measured offset in diagnostics.

---

## 7. Integration points — every file you must touch

### 7.1 `DeviceOverride` — `src-tauri/src/streamer/session.rs:26`

```rust
pub struct DeviceOverride {
    pub scale: u32,
    pub orientation_portrait: bool,
    pub refresh_rate: u32,
    pub video_scale: u32,
    pub video_quality: u8,
    pub control_enabled: bool,
    pub audio_enabled: bool,   // NEW — default false
}
```

**Default `false`.** Audio is a privacy-relevant capability (it captures everything the host plays,
including other people's calls) and should be opt-in per device.

**Sequencing note:** there is queued work to re-key `SharedDeviceOverrides` from IP to a per-device
token. If that lands in the same window, **do it first** — otherwise this field gets migrated
twice. Check the current state of `is_ip_approved` / `SharedDeviceOverrides` before starting.

### 7.2 Apply block — `src-tauri/src/streamer/server.rs:526-535`

Mirror how `control_enabled` is read:
```rust
let control_enabled = override_for_ip.map(|o| o.control_enabled).unwrap_or(true);
```
Add `audio_enabled`, defaulting to `false`, and thread it into `handle_whep_offer` alongside
`control_enabled` (currently passed at `server.rs:696`, signature at `webrtc_session.rs:263-270`).

### 7.3 `JoinRequest` — `src-tauri/src/streamer/server.rs:24`

Add client capability declaration so the host picks the right path:
```rust
#[serde(default, rename = "audioCapabilities")]
audio_capabilities: AudioCapabilities,   // { webcodecs_opus: bool, sab: bool, worklet: bool }
```
`#[serde(default)]` so older clients still parse. Apply the same input-validation discipline the
other fields need — bound anything unbounded.

### 7.4 Config — `src-tauri/src/streamer/config.rs`

Add shared audio state following the existing `Option<Shared*>` pattern (see `virtual_display`,
`device_overrides`, `disconnect_grace`). The capture handle needs the same lifecycle treatment
sessions get: started on first enabled device, stopped when the last one leaves.

### 7.5 Session lifecycle — `src-tauri/src/streamer/session.rs`

`SessionState` tracks `active_capture` for video with a stop function. Do the same for audio.
Critically: **the audio capture is a single shared host-wide stream**, not per-device — one WASAPI
loopback client fans out to N encoders or, better, one encoder fanning out to N data channels if
all devices use the same settings. Reference-count it. Do not open N loopback clients.

### 7.6 CLI — `src-tauri/tauri.conf.json` + `src-tauri/src/cli.rs`

Add to the `devices set` subcommand, next to the existing `control` arg:
```json
{ "name": "audio", "takesValue": true, "possibleValues": ["on", "off"],
  "description": "System audio streaming." }
```
Wire it in `cli.rs` where `control` is handled, and include it in `devices list` output (both human
and `--json`).

### 7.7 Desktop UI — `src/components/pages/device-details.tsx`

Add the toggle **directly below the remote control toggle**, matching its exact markup, spacing,
and the `Switch` component already used. Label: "System audio". Sub-label should state plainly what
it does — something like "Streams this computer's audio output to the device."

Also update:
- `src/lib/bindings.ts` — **regenerate** via `pnpm tauri dev`; do not hand-edit.
- `src/i18n/locales/en.json` — add the strings; do not hardcode English in the component.
- `src/pages/devices.tsx` — if the device list shows capability badges, add audio.

### 7.8 Client page — `src-tauri/src/streamer/static/index.html`

- Capability detection for `AudioDecoder`, `AudioWorklet`, `SharedArrayBuffer`; report in the join
  request (§7.3). The file already has a compatibility-check block (around lines 1010–1035) —
  extend that rather than writing a parallel one.
- Mute/unmute control, defaulting to **unmuted** when the host has enabled audio.
- Resume the `AudioContext` on the existing OTP-submit gesture.
- Show which path is active (§6.3).
- If you touch the `innerHTML` assignment at line 1029, convert it to DOM construction while
  you're there — the surrounding code already uses `textContent`/`createElement`.

### 7.9 Cargo — `src-tauri/Cargo.toml`

Under `[target.'cfg(windows)'.dependencies]`, add the `windows` crate features you need:
`Win32_Media_Audio`, `Win32_Media_Audio_Endpoints`, `Win32_System_Com`,
`Win32_System_Threading`, `Win32_Media_KernelStreaming`, and `Win32_System_Performance` for QPC.
Check what's already enabled before adding — the windows-capture code enables a lot. Add `avrt` for
MMCSS if it's behind its own feature.

`libloading` and `crossbeam-channel` are already dependencies. Do not add an audio crate.

### 7.10 Stubs — macOS and Linux

- `src-tauri/src/macos_utils/audio.rs`
- `src-tauri/src/linux_utils/audio.rs`

Same public surface as the Windows module; return a clear "system audio is not supported on this
platform" error. Must compile. The UI toggle should be **visible but disabled** with a tooltip
explaining why, not hidden — hiding it makes the feature look broken rather than unimplemented.

Follow the existing pattern in `linux_utils/streamer.rs` and `macos_utils/` for how the repo stubs
unimplemented platform surfaces.

For macOS, leave a comment noting the eventual path (ScreenCaptureKit already captures system audio
and `macos_utils/streamer/sck.rs` exists) — but do not implement it now.

---

## 8. Conventions — non-negotiable

From `CLAUDE.md`, and verified as currently 100% observed in this codebase:

1. **`std::sync::Mutex` only** for first-party code. No `parking_lot` outside
   `windows_utils/windows_capture/`.
2. **Never hold a `MutexGuard` across `.await`.** Including the temporary-lifetime case where an
   `if let` / `match` scrutinee holds the guard to the end of the block. The codebase currently has
   **zero** violations — verified by scanning every such block. Do not introduce the first one.
3. The audio capture thread is a **dedicated OS thread**, not a tokio task. It's a real-time
   priority thread with MMCSS. Communicate with the async world over `crossbeam-channel`, the way
   the video pipeline does.
4. **Never allocate or lock in the capture callback.** Preallocate everything.
5. Register any new Tauri commands/events in `collect_commands!` / `collect_events!` in
   `src-tauri/src/lib.rs::run`, then regenerate bindings.
6. `// SAFETY:` on every `unsafe` block. RAII wrappers for every COM pointer, encoder handle, MMCSS
   handle, and event handle.
7. Conventional commits matching the existing log: `feat(windows): ...`, `fix(audio): ...`.

---

## 9. Diagnostics and measurement

You cannot claim the latency target without measuring it. Build the instrumentation as you go, not
at the end.

**Host-side counters**, exposed through the existing log bus (`src-tauri/src/logbus.rs`) and
visible in the Settings page's live log:
- Actual negotiated capture period (ms) and mix format
- Encode time per frame: p50 / p99
- `DATA_DISCONTINUITY` count, silent-packet count
- DataChannel `buffered_amount` and dropped-packet count
- Device-change events

**Client-side overlay** (extend whatever the video path already exposes):
- Current jitter buffer depth (ms), underrun/overrun counts
- Decode time per packet
- Measured A/V offset (ms)
- Which path is active: WebCodecs or NetEQ fallback

**End-to-end measurement procedure.** Document it in `AUDIO_NOTES.md` so the number is
reproducible: play a click track on the host, capture both host output and client output on one
recording device, measure the offset. Report the number. If it exceeds 35 ms, say so and explain
where the budget went rather than quietly shipping.

**Tests:**
- `test/format.rs` — format conversion, downmix matrix, int→float edge cases (int16 min value,
  clipping)
- `test/opus_layout.rs` — FFI constants and struct layout vs. the header
- Protocol round-trip: encode header → parse header
- Jitter buffer: underrun, overrun, reordering, duplicate sequence numbers
- Mock capture source so the pipeline is testable without audio hardware — see
  `windows_utils/driver_ipc/mock.rs` for the repo's mocking pattern

---

## 10. Definition of done

- [ ] `./generate-sources.sh` run; `.sources/` present and gitignored
- [ ] `AUDIO_NOTES.md` records all six §3 spike answers with measured numbers
- [ ] Toggle appears below remote control in Edit Device; persists; takes effect without
      reconnecting the device
- [ ] `audio_enabled` defaults to **false**
- [ ] Audio streams on Windows 19041 with no build-gated API in the required path
- [ ] Follows default-device changes without dropping the connection
- [ ] Produces continuous silence when the host is idle (no stall, no timeline gap)
- [ ] Measured end-to-end latency reported, with the §9 procedure documented
- [ ] A/V offset within ±20 ms
- [ ] Fallback path works and is visibly reported when active
- [ ] macOS/Linux compile; toggle disabled with an explanatory tooltip
- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, `pnpm build` all pass
      (state your platform — most of this is `cfg`-gated and unbuildable on Linux)
- [ ] `libopus.dll` provenance recorded with SHA-256 and license
- [ ] README updated: audio in Features, and the privacy implication stated plainly
- [ ] `CLAUDE.md` updated with the new module's layout and the capture-thread convention

---

## 11. If you get stuck

**Say so, with specifics.** Do not substitute a rejected approach from §2 to make progress.

The three most likely walls, and what to do:

1. **Loopback won't do low-latency shared mode.** Report the measured floor from `IAudioClient3`
   and what the fallback `Initialize` path actually achieves. If the device minimum is 10 ms, the
   budget in §1 needs revising — say that rather than quietly missing it.
2. **`SharedArrayBuffer` unavailable and HTTPS isn't landed.** Take §6.4 option 2, ship it, and
   flag the latency cost.
3. **A/V sync drifts and won't hold.** This is the hardest part. Get the clocks onto one timebase
   before trying to correct drift — most sync bugs are actually two-timebase bugs.

Escalate with the measurement, not just the symptom.
