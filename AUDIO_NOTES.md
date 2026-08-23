# AUDIO_NOTES.md — system-audio spike results & design decisions

Working notes for the per-device system-audio feature (PRD `PRD.md`). The §3 spike answers
below are **measured**, not recalled. Reproduce with:

```sh
cd src-tauri
cargo run --example audio_spike
```

The example (`src-tauri/examples/audio_spike.rs`) is a throwaway harness, Windows-only, not
part of the shipping build.

## Measurement host

- Windows 11 Home 10.0.26200 (dev machine).
- Default render endpoint mix format: **48000 Hz, 2ch, 32-bit IEEE_FLOAT, WAVE_FORMAT_EXTENSIBLE**
  (`nBlockAlign=8`). This is the 48 kHz fast path — no resample needed.

> Numbers below are from this one machine. The capture-period floor in particular is
> hardware/driver dependent; §3.3 is exactly why we query it at runtime instead of assuming.

---

## §3.1 — Does `AUDCLNT_STREAMFLAGS_LOOPBACK` work with `IAudioClient3::InitializeSharedAudioStream`?

**No.** Measured:

```
IAudioClient3::InitializeSharedAudioStream(LOOPBACK|EVENTCALLBACK, min_period)
  FAILED: 0x88890021   (AUDCLNT_E_INVALID_STREAM_FLAG)
```

The low-latency shared path rejects the loopback flag outright. The documented supported-flags
list for `InitializeSharedAudioStream` really is a subset, and loopback is not in it.

**Fallback (measured to work):** `IAudioClient::Initialize(AUDCLNT_SHAREMODE_SHARED,
AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, mixformat, null)`.
Succeeds. Engine buffer came back at 1056 frames (22 ms); effective delivery cadence ≈ 480
frames (10 ms) per packet (see §3.2).

**Decision:** the capture client uses the **legacy `IAudioClient::Initialize` loopback path**.
`IAudioClient3` is used only to *query* `GetSharedModeEnginePeriod` for diagnostics; we do not
initialize through it. This matches OBS's `win-wasapi.cpp`, which also uses legacy `Initialize`
for loopback.

## §3.2 — Does event-driven loopback (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`) actually signal?

**Only when a render stream is active on the endpoint.** Measured, 20 iterations × 200 ms wait:

| Scenario | Event signals | Timeouts | Packets | Notes |
| --- | --- | --- | --- | --- |
| Host idle, **no** companion | **0 / 20** | 20 | 0 | event never fires; loopback delivers *nothing* |
| Host idle, **with** silent render companion | **20 / 20** | 0 | 19 | all silent packets; interval avg 9.39 ms, max 10.29 ms |

This confirms the §4.3 idle-endpoint problem empirically: with nothing playing, the Windows
audio engine parks the endpoint and loopback capture produces no packets *and* the event never
fires. A **silent render companion stream** on the same endpoint is required — not merely to
avoid idle silence gaps, but as the **clock source that makes the capture event fire at all.**

Also observed: the first packet after start carries `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`
(count=1), and companion-driven idle packets carry `AUDCLNT_BUFFERFLAGS_SILENT`.

**Decision:** `silence.rs` (silent render companion) is mandatory whenever capture is running,
started lazily on the first audio-enabled device and stopped when the last one leaves. The
capture loop waits on the capture stream's own event with a period-sized timeout as a safety
net (mirrors OBS's short-timeout poll fallback).

## §3.3 — `GetSharedModeEnginePeriod` minimum on this hardware

```
default     = 480 frames (10.000 ms)
fundamental = 480 frames (10.000 ms)
minimum     = 480 frames (10.000 ms)   <- capture-latency floor
maximum     = 480 frames (10.000 ms)
```

This endpoint exposes **only** the standard 10 ms engine period — there is no sub-10 ms
low-latency variant here (and since §3.1 rules out the IAudioClient3 loopback path anyway, the
point is moot for capture). The real, measured loopback packet cadence was ~9.4 ms avg /
10.3 ms max, i.e. one 10 ms packet per period.

**Latency-budget consequence (per PRD §11.1):** the PRD §1 table budgets "3–10 ms" for the
WASAPI loopback period. On this hardware it is pinned at the **top of that range: 10 ms.**
The end-to-end ≤ 35 ms target is still reachable but tight:

| Stage | PRD target | Measured/expected here |
| --- | --- | --- |
| WASAPI loopback period | 3–10 ms | **10 ms** (hardware floor) |
| Opus encode (CELT, 5 ms frame) | ~8 ms | ~6.5 ms lookahead + <1 ms encode |
| Network (LAN) | 1–5 ms | 1–5 ms |
| Client jitter buffer | 10–15 ms | 10–15 ms (ours) |
| AudioContext output | 3–10 ms | 3–10 ms |

Sum ≈ 30–46 ms. The 10 ms capture period is the single biggest fixed cost and is not something
we can shrink on this box. Reported honestly rather than silently missed. End-to-end
click-track measurement procedure is in §9 / below; the number will be filled in when measured
on real playback hardware.

## §3.4 — Does Chrome's `AudioDecoder` accept raw Opus packets without an `OpusHead` description?

**Yes — no `description` needed for raw Opus.** Authoritative source: the W3C WebCodecs Opus
codec registration (`.sources/docs/web/webcodecs-codec-registry.html`):

> "If a `description` has been set, the bitstream is assumed to be in `ogg` format. If a
> `description` has not been set, the bitstream is assumed to be in `opus` format."

So configuring `AudioDecoder` with `{ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 }`
and **omitting** `description` tells it to expect raw Opus packets (RFC 6716), which is exactly
what our encoder emits. Setting a `description` would instead switch it to Ogg-encapsulated
mode — wrong for us.

- **Chrome / Edge:** raw Opus, no description. ✅
- **Safari:** WebCodecs `AudioDecoder` Opus support is the likely gap; the client
  capability-detects `AudioDecoder` + `AudioWorklet` + usable `SharedArrayBuffer` and falls
  back to the standard WebRTC audio track (§6.3) when any is missing. We do **not** synthesize
  an OpusHead because the primary path never needs one.

(Browser-side; confirmed from the spec text, not run through a Rust harness.)

## §3.5 — Smallest Opus frame the client path handles cleanly

**Default 5 ms (240 samples @ 48 kHz).** 2.5 ms is theoretically lowest but doubles the packet
rate — per-packet SCTP/DataChannel header overhead and per-callback worklet cost make it a poor
trade below 5 ms. 10 ms halves packet rate at the cost of +5 ms buffering. 5 ms is the PRD
default and the balance point.

This one is genuinely a *client-measured* quantity (per-callback cost varies by device); the
client diagnostics overlay (§9) reports decode-time-per-packet and jitter depth so the frame
size can be revisited on real devices. Encoder frame size is a single constant
(`FRAME_MS`/`FRAME_SAMPLES` in `windows_utils/audio/encoder.rs`) so changing it is one edit.

## §3.6 — Is `SharedArrayBuffer` available? (couples the fast path to HTTPS)

**Only in a cross-origin-isolated secure context.** Requirements (confirmed from
`.sources/docs/web/cross-origin-isolation.html` and `sharedarraybuffer.md`):

1. Response headers on the document (and its workers):
   - `Cross-Origin-Opener-Policy: same-origin`
   - `Cross-Origin-Embedder-Policy: require-corp`
2. A **secure context**: HTTPS, or `localhost`/`127.0.0.1`/`[::1]` over HTTP.

The join flow currently defaults to plaintext `http://` off-localhost (`src/lib/utils.ts`),
which is **not** a secure context, so `SharedArrayBuffer` / `crossOriginIsolated` is false there.

**Decision (PRD §6.4):** the HTTPS-first join migration is not landed, so we take **option 2**:
a **non-SAB ring buffer** — the decode worker `postMessage`s `Float32Array`s (transferred) into
the worklet. Slightly higher and less predictable latency + more GC pressure, but it works over
plain HTTP on the current join URL. We still:
  - add the COOP/COEP headers to the axum responses (so the SAB path lights up automatically
    once HTTPS-first lands), and verify they don't break the existing video worker loading;
  - capability-detect `SharedArrayBuffer` + `crossOriginIsolated` client-side and prefer the SAB
    ring buffer when available, falling back to the postMessage ring otherwise;
  - surface which transport/decoder path is active in the client overlay so a silent downgrade
    is impossible to miss.

---

## Design summary (what the spike settled)

- **Capture:** legacy `IAudioClient::Initialize` shared-mode loopback + `EVENTCALLBACK`, on the
  `eRender`/`eConsole` default endpoint. `IAudioClient3` used only to *report* the engine period.
- **Silent render companion is mandatory** and is the clock source, not just an idle-silence
  patch. Start lazily, stop on last leave.
- **48 kHz float32 stereo is the fast path** on this host — pass straight to Opus, zero
  conversion. Non-48k/other layouts handled per §4.4 (`AUTOCONVERTPCM` first, downmix matrix,
  int→float) but not exercised on this hardware.
- **Opus:** `OPUS_APPLICATION_RESTRICTED_LOWDELAY`, 5 ms frames, CELT-only.
- **Client fast path:** WebCodecs `AudioDecoder` (no description) → ring buffer → `AudioWorklet`;
  **non-SAB postMessage ring for now** because HTTPS-first isn't landed. NetEQ track fallback
  when WebCodecs/worklet/SAB unavailable.
- **Capture-period floor is 10 ms on this box**; the ≤ 35 ms end-to-end budget is tight but
  reachable. Report the real click-track number when measured on playback hardware; do not
  claim the target from the model alone.

## End-to-end latency measurement procedure (per §9)

1. On the host, play a periodic click track (e.g. a 1 kHz tick every 500 ms) through the default
   output device.
2. Join from a client on the same LAN with audio enabled (fast path).
3. With a single recording device, capture **both** the host speaker output and the client
   speaker output simultaneously (one mic in front of both, or a loopback+line mix).
4. In an audio editor, measure the offset between the host click and the corresponding client
   click. That offset is capture-to-speaker end-to-end latency.
5. Cross-check A/V: with video also on screen, read `SEAudio.getSyncInfo().residualOffsetMs` in
   the client console (§6.5) — it should sit near 0 once locked. For an acoustic check, film both
   the host click and the on-screen client picture and measure the audio-vs-picture offset.

Report the measured number here once taken on real playback hardware. If it exceeds 35 ms,
attribute where the budget went (expected dominant term: the 10 ms capture period + client
jitter buffer) rather than quietly shipping.

**Status:** capture-period / event-behavior / mix-format numbers are measured (above). The
click-track end-to-end number requires two-device acoustic capture and is pending that hardware
setup; the procedure is fixed so the figure is reproducible.

## Build / verification status (this machine: Windows 11 26200)

- `cargo fmt --check` — clean.
- `cargo clippy --all-targets -- -D warnings` — clean (lib, bins, tests, `examples/audio_spike`).
- `cargo test --lib --bins --tests` — 127 passed, 1 ignored (the pre-existing DXGI probe), 0 failed.
  (Plain `cargo test` also runs the vendored `windows_capture` fork's doctests, which fail with
  `E0433: cannot find crate windows_capture` — their examples `use windows_capture::…`, i.e. the
  upstream crate name, which doesn't resolve once the fork is vendored as a module. Pre-existing,
  unrelated to this feature, and out of scope per CLAUDE.md's "leave the fork as-is".)
- `pnpm build` (tsc + vite) — clean.
- Static client JS (`audio.js`, `audio-worklet.js`, `transform-worker.js`) — `node --check` clean
  (they have no bundler step; served as-is by the Rust server).
- libopus FFI validated end-to-end against the bundled `resources/libopus.dll`: create with
  `RESTRICTED_LOWDELAY`, the two-typed-views variadic `opus_encoder_ctl` (SET int / GET ptr),
  and `opus_encode_float` of a 5 ms stereo frame → 118 bytes, `GET_LOOKAHEAD` = 120 samples.

**`src/lib/bindings.ts`:** this is normally regenerated by `pnpm tauri dev` (tauri-specta on a
debug run). That launches the desktop app, which isn't possible in this headless build session,
so the two deltas (`setDeviceOverride`'s new `audioEnabled` param and `Device.systemAudio`) were
hand-applied to exactly match tauri-specta's deterministic output. **Run `pnpm tauri dev` once to
regenerate authoritatively** — the output will be identical. Everything downstream (`pnpm build`)
already type-checks against it.

## A/V sync (§6.5) — implemented

Both media now ride **one host timebase** (`HOST_EPOCH` in `streamer/audio/mod.rs`):

- **Audio** carries the host `capture_ns` in its DataChannel header (unchanged).
- **Video** now carries the same clock **in the RTP timestamp**. The video track was switched
  from `TrackLocalStaticSample` to a raw `TrackLocalStaticRTP` (`webrtc_session.rs`) so we can
  stamp every packet's 90 kHz timestamp with `host_ns_to_rtp90k(host_instant_to_ns(frame.capture))`
  instead of the crate packetizer's random base (which the client can't map to host time). We
  reuse the crate's `H264Payloader` + sequencer, so payloading/marker/SSRC/PT and the
  interceptor-based NACK/RTX are unchanged. BWE is unaffected — this host derives it from
  `getStats` (`bytes_sent`, `fraction_lost`), not from `abs-send-time`/REMB, and the default
  interceptor set has no sender-side TWCC/abs-send-time interceptor anyway.

Client alignment (no absolute clock handshake needed — both media share the clock):

- `transform-worker.js` inverts each drawn frame's RTP timestamp back to host-capture time
  (unwrapping the u32 across its ~13.25 h wrap) and reports an EMA of the **display lag**
  (`drawAbs − videoHostMs`) to the main thread.
- `audio.js` measures its own **capture→enqueue latency** per packet (`enqueueAbs − captureHostMs`,
  from the decoded `AudioData.timestamp`) and commands the worklet a **target buffer depth** so
  `preEnqueue + depth + outputLatency == videoDelay`. The `(client−host)` epoch gap cancels
  between the two delays; the desired depth is reduced mod the RTP wrap period so it stays correct
  even if the 64-bit audio clock and the wrapping 90 kHz video clock disagree on wrap count.
- `audio-worklet.js` holds that depth and corrects residual drift **only across silent stretches**
  (peak < ~−49 dBFS): drop a little when too deep, insert a silent block when too shallow — never
  cutting a tone (§6.5). Falls back to its adaptive 10–40 ms behavior when no video delay is known
  (audio-only) or on the NetEQ fallback path (which times itself).

**Measured offset (diagnostics):** `SEAudio.getSyncInfo()` returns `{videoDelayMs, preEnqueueMs,
targetMs, residualOffsetMs}` and the residual is logged every ~2 s. `residualOffsetMs` is the
sync error left after clamping the target to [10, 400] ms — ≈0 when locked; nonzero only when the
required buffering falls outside those bounds (e.g. audio path slower than video). The
capture-thread convention and the previous overlay were removed per the client-page
visual-indicator cleanup, so the number surfaces via `getSyncInfo()` / console, not an on-screen HUD.

**Still pending real hardware:** the ±20 ms acceptance figure needs the two-device acoustic
capture in the procedure above; the mechanism is in place and self-consistent, but the on-air
offset number should be confirmed on real playback + display hardware.

**Not wired (deliberate):**
- Process loopback (per-app audio, build 20348+) is out of scope (§2); capture uses endpoint
  loopback only.
