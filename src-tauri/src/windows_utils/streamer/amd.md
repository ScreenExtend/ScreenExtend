# AMD AMF encoder backend — design & implementation guide

> **North-star: minimize glass-to-glass latency. Period.** This backend exists to give AMD
> Radeon / Ryzen-iGPU machines the *same* ultra-low-latency path the NVENC backend
> (`nvidia/encoder.rs`) gives NVIDIA: **D3D11-native, zero-copy, one frame in flight, ~1-frame
> VBV, no B-frames, no lookahead, no pre-analysis.** Every setting below is chosen for latency
> first and quality second. If a knob trades 1 ms for a little PSNR, we spend the PSNR.

It implements `crate::platform::EncoderBackend` for AMD GPUs via **AMF (Advanced Media
Framework)**, AMD's hardware-encode runtime. Treat this as the spec for `amd.rs`; it is the AMD
mirror of the rationale block atop `nvidia/encoder.rs`.

---

## 1. Binding strategy — no SDK at build time

Same philosophy as NVENC: **do not** link an SDK import library or require an SDK install.

- The AMF runtime ships **with the Radeon driver** as `amfrt64.dll` (system path). We
  **dynamically load** it (`libloading`) and resolve the two C entry points it exports:
  `AMFInit(version, **ppFactory)` and `AMFQueryVersion(*pVersion)`.
- From the returned `AMFFactory` everything else is obtained through **vtable calls** on COM-like
  `AMFInterface` objects (`AMFFactory`, `AMFContext`, `AMFComponent`, `AMFSurface`, `AMFBuffer`,
  `AMFData`, `AMFVariant`). We vendor only the **AMF headers** (`core/`, `components/`) — MIT,
  redistributable — and hand-translate the handful of structs/enums/vtables we touch into a
  `amf_sys/` module exactly like `nvidia/nvenc_sys/`. No `amfrt64.lib`, no toolkit, no `LIBCLANG`.
- Version gate: require `AMFQueryVersion` ≥ the build's `AMF_FULL_VERSION`; log and fall back to
  the next vendor if the driver is too old (ULL + intra-refresh need a reasonably recent driver).

`amd_sys/` layout mirrors `nvidia/nvenc_sys/`: `mod.rs` (loader + vtable typedefs), plus
hand-maintained enum/const files. Keep the rationale note at the top, like NVENC.

---

## 2. Architecture (data path, all on-GPU)

```
WGC capture (BGRA, ID3D11Texture2D, on the display's adapter)
   │
   │  [same-adapter]                         [cross-adapter: capture iGPU ≠ encode AMD dGPU]
   │  zero copy                              CopyResource into a SHARED_NTHANDLE|KEYEDMUTEX texture
   ▼                                         (reuse pipeline.rs build_zero_copy pattern)
D3D11 VideoProcessor  (scaler.rs, extended) ── BGRA → **NV12** (+ optional downscale) in ONE pass
   ▼
AMFSurface wrapping the NV12 ID3D11Texture2D   (context->CreateSurfaceFromDX11Native — ZERO COPY)
   ▼
AMFComponent  AMFVideoEncoderVCE_AVC  (SubmitInput → QueryOutput, single frame in flight)
   ▼
AMFBuffer (Annex-B H.264)  → Bytes → broadcast::Sender<EncodedFrame>  (unchanged transport)
```

Key architectural decisions, all latency-driven:

- **Share the capture `ID3D11Device`.** `context->InitDX11(device, AMF_DX11_1)` on the *same*
  device that owns the captured texture, so surface import is a pointer wrap, not a copy. AMF
  honors an externally-provided D3D11 device — use it, never let AMF create its own.
- **Same-adapter is the common AMD case and the fastest.** On a Radeon desktop (or a Ryzen APU
  laptop) the display, capture, *and* VCE encoder are all on one adapter → the cross-adapter copy
  the NVENC path needs on this hybrid laptop **disappears entirely**. Detect this and skip the
  shared-texture dance. Only fall back to the keyed-mutex cross-adapter copy when capture and AMD
  encode are genuinely on different adapters.
- **Color convert is mandatory and free-ish.** VCE wants **NV12**; WGC gives BGRA. Do the
  BGRA→NV12 conversion in the **D3D11 VideoProcessor** (`scaler.rs`, taught to emit NV12) so
  scale + color-convert are a **single GPU pass** feeding a texture AMF wraps directly. Do **not**
  use a CPU convert, and prefer the VideoProcessor over AMF's own `AMFVideoConverter` component so
  we keep one well-understood GPU pass instead of two.
- **One frame in flight.** AMF is async (`SubmitInput`/`QueryOutput`). We submit exactly one
  surface, then `QueryOutput` for it before submitting the next — no encode queue to drain, same
  latest-wins discipline as the NVENC path. Set `AMF_VIDEO_ENCODER_QUERY_TIMEOUT = 0`
  (non-blocking) and spin a tight bounded poll; never block on a deep async pipeline.
- **Annex-B output.** AMF AVC emits Annex-B start codes. Force SPS/PPS in-band on every IDR (see
  header-insertion setting) so a mid-stream joiner / post-PLI IDR is self-contained.

---

## 3. Encoder settings — the latency table

Set on the `AMFComponent` **before** `Init`, unless marked per-frame. Names are real AMF property
IDs (`AMF_VIDEO_ENCODER_*`). The "why" is always latency.

| Property | Value | Why (latency) |
|---|---|---|
| `AMF_VIDEO_ENCODER_USAGE` | `ULTRA_LOW_LATENCY` | The master switch. Presets ULL-friendly defaults (no B, tight RC). Set this **first** — other USAGE values re-default the encoder for throughput/quality and re-introduce buffering. |
| `AMF_VIDEO_ENCODER_QUALITY_PRESET` | `SPEED` | Fastest VCE algorithm path = lowest encode time. We buy latency with quality; CBR keeps it watchable. |
| `AMF_VIDEO_ENCODER_LOWLATENCY_MODE` | `true` | Explicit low-latency mode on recent drivers; tightens the internal submit→output path. Probe-and-set (older drivers lack it). |
| `AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD` | `CBR` | Constant bitrate = predictable per-frame size = predictable pacing. VBR/CQP let frames balloon and stall the link. |
| `AMF_VIDEO_ENCODER_RATE_CONTROL_PREANALYSIS_ENABLE` | `OFF` | Pre-analysis is a lookahead pass — pure added latency. Off. |
| `AMF_VIDEO_ENCODER_PREENCODE_ENABLE` | `OFF` | Pre-encode (a first analysis pass) is another lookahead. Off. |
| `AMF_VIDEO_ENCODER_B_PIC_PATTERN` | `0` | **No B-frames.** B-frames need future frames → reorder delay of ≥1 frame. Forbidden. |
| `AMF_VIDEO_ENCODER_MAX_NUM_REFRAMES` | `1` | Single reference. More refs = more DPB management, no latency upside for a live wall-clock stream. |
| `AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE` | `bitrate / fps` (≈ **1 frame** of bits) | The single biggest CBR latency lever. A 1-frame VBV means the rate controller can't bank bits and stall; it matches NVENC's ~1-frame VBV. |
| `AMF_VIDEO_ENCODER_INITIAL_VBV_BUFFER_FULLNESS` | `0` (full / no pre-fill) | No initial buffering delay before the first frame leaves. |
| `AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE` | `OFF` | Filler enforces strict CBR by padding bytes — wasted bytes = wasted transmit time on our link. Let CBR run without filler. |
| `AMF_VIDEO_ENCODER_TARGET_BITRATE` / `_PEAK_BITRATE` | equal, = target | Peak == target so the controller can't spike a frame above budget and HOL-block the next. |
| `AMF_VIDEO_ENCODER_IDR_PERIOD` | very large (≈ "infinite") | No periodic IDR. Keyframes are **on demand only** (join / PLI / FIR). Periodic IDRs are bitrate spikes = latency spikes. |
| `AMF_VIDEO_ENCODER_GOP_SIZE` | very large | Same intent — open GOP-free, recover via intra-refresh, not scheduled IDRs. |
| `AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT` | sized for a **~1 s refresh wave** | Rolling intra-refresh heals loss *passively* with no full-IDR spike — the latency-favoring recovery the whole project is built around (mirrors NVENC `intra_refresh`). |
| `AMF_VIDEO_ENCODER_SLICES_PER_FRAME` | `4`–`8` | Slice-based output: a lost packet corrupts one slice's region, not the whole frame, and slices can be packetized as they complete. Matches NVENC's 4–8 slices. |
| `AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING` | emit SPS/PPS on every IDR | In-band parameter sets so a joiner/post-PLI IDR decodes standalone (no out-of-band negotiation round trip). |
| `AMF_VIDEO_ENCODER_PROFILE` | `CONSTRAINED_BASELINE` (or `MAIN`) | Baseline avoids CABAC → cheaper, more pipelineable **decode** on the receiver. Match the NVENC default; only go High if the receiver decode budget allows. |
| `AMF_VIDEO_ENCODER_FRAMERATE` | actual fps (num/den) | Correct fps so CBR sizes per-frame bits right; wrong fps mis-sizes the VBV and adds jitter. |
| `AMF_VIDEO_ENCODER_QUERY_TIMEOUT` | `0` | Non-blocking `QueryOutput` so the encode thread never parks waiting on a deep pipeline. |
| `AMF_VIDEO_ENCODER_DE_BLOCKING_FILTER` | on (default) | Leave on — it's free on the encoder and improves the picture the receiver shows; it is not a latency cost. |

**Per-frame (on the `AMFSurface` before `SubmitInput`):**

| Property | When | Why |
|---|---|---|
| `AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE = IDR` | join / inbound PLI / FIR | On-demand keyframe — the *only* keyframe trigger. Wire to the same `idr_request` flag the pipeline already exposes. |
| `AMF_VIDEO_ENCODER_INSERT_SPS` / `_INSERT_PPS = true` | on each forced IDR | Guarantee the IDR carries its parameter sets. |

---

## 4. Dynamic controls (no teardown — teardown is a latency event)

- **Adaptive bitrate** (`set_bitrate`): set `AMF_VIDEO_ENCODER_TARGET_BITRATE` **and**
  `_PEAK_BITRATE` live on the running component — AMF applies it without a rebuild. Drive it from
  the existing TWCC-ish controller via `Pipeline::set_target_bitrate`. **Never** change resolution
  at runtime (the input/NV12/encoder surfaces are pinned at init, exactly like NVENC — a resize is
  a full teardown + IDR spike, which is the latency we are removing).
- **Force IDR**: per-surface property above; same semantics as NVENC `force_idr()`.

---

## 5. Milestones (gate one at a time; each must compile, run, and pass before the next)

Mirror the project's M-gating. Suggested CLI probes parallel the NVENC ones (`--probe-encode`,
`--probe-live`, `--whep-selftest`), selected onto the AMD backend.

- **M-AMD0 — load & device.** Dynamically load `amfrt64.dll`, `AMFInit`, create factory + context,
  `InitDX11` on a throwaway D3D11 device, create the `AMFVideoEncoderVCE_AVC` component, `Init`,
  destroy cleanly. **Test:** runs without error and logs the AMF version + selected adapter; no
  leaks on repeat (loop it 100×).
- **M-AMD1 — NV12 convert.** Extend `scaler.rs` to output NV12; convert a known BGRA test texture
  and dump the NV12 to disk. **Test:** `ffplay -f rawvideo -pix_fmt nv12 -s WxH out.nv12` shows the
  correct image; bytes match an expected checksum for the synthetic pattern.
- **M-AMD2 — encode to file (ULL).** Wrap the NV12 texture as an `AMFSurface`, encode 300 synthetic
  frames with the §3 settings to `out_amd.h264`. **Test:** `ffprobe` confirms **AVC, no B-frames**
  (`has_b_frames=0`), SPS/PPS present, slice count 4–8; `ffplay out_amd.h264` plays; measured
  encode time/frame logged and **≤ NVENC's ~8 ms at the same resolution** (sanity, not a hard gate).
- **M-AMD3 — zero-copy import.** Replace any staging copy with
  `CreateSurfaceFromDX11Native` on the VideoProcessor's NV12 output; assert **no CPU readback** on
  the hot path. **Test:** a debug counter shows zero `Map`/`memcpy` per frame; encode-time/frame
  drops vs M-AMD2; PIX/GPUView shows no CPU-GPU round trip.
- **M-AMD4 — same-adapter live.** Wire into the live pipeline on a machine where the AMD GPU owns
  the captured display: WGC → VP(NV12) → AMF → track. **Test:** `--probe-live` writes 150 clean
  frames; the WHEP self-test asserts RTP flows; on-screen latency looks instant.
- **M-AMD5 — cross-adapter fallback.** Handle capture-iGPU ≠ AMD-dGPU via the keyed-mutex shared
  texture (reuse `pipeline.rs::build_zero_copy`); log which path was chosen. **Test:** force the
  fallback, confirm 150 clean frames and a single GPU→GPU copy (no CPU bounce).
- **M-AMD6 — dynamic bitrate + IDR.** Live `TARGET/PEAK_BITRATE` updates and forced IDR on PLI.
  **Test:** an AMD analog of `--probe-bitrate` steps the target down/up and `ffprobe`/byte-rate
  tracking shows the stream following within a few frames **with no resolution change and no IDR
  storm**; an injected PLI produces exactly one IDR.

---

## 6. How to test latency (the only metric that matters)

- **Stream sanity:** `ffprobe -show_frames out_amd.h264` → assert `pict_type` is only I/P (never B),
  `has_b_frames=0`, and that SPS/PPS reappear at each IDR.
- **Encode-path latency:** reuse the pipeline's existing per-60-frame avg/max encode-time log; AMD
  must land in the same band as NVENC (~single-digit ms at 1080p, low-teens at 4K).
- **Glass-to-glass:** the project's §16 photographic method — a millisecond stopwatch on the
  captured monitor photographed next to the receiving browser; compare AMD vs NVENC on the same
  box. Confirm hardware decode in `chrome://media-internals`.
- **No-spike check:** run 5+ minutes and watch the encode-time **tail** (max), not just average —
  a creeping max means a buffering/VBV regression. The tail is where latency bugs hide.

---

## 7. Latency traps to avoid (AMF-specific)

- Setting `USAGE` to anything but `ULTRA_LOW_LATENCY`, or setting it *after* other RC props (it
  re-defaults them) → silent re-introduction of buffering.
- Letting AMF allocate its **own** D3D11 device/queue instead of sharing capture's → a hidden
  cross-context copy every frame.
- Submitting more than one surface before querying output → an encode queue forms; wall-clock
  latency grows by queue depth × frame time even though throughput looks fine.
- Leaving `PREANALYSIS`/`PREENCODE`/B-frames on (some presets enable them) → ≥1 frame of
  algorithmic delay.
- A VBV larger than ~1 frame, or `FILLER_DATA` on → CBR banks bits and stalls; or pads bytes that
  cost transmit time.
- CPU NV12 conversion, or AMF's converter as a *second* GPU pass when the VideoProcessor can do
  scale+convert in one.

---

## 8. References
- Vendored **AMF headers** (`AMF/amf/public/include/...`) — `components/VideoEncoderVCE.h`
  (every `AMF_VIDEO_ENCODER_*` property + enum), `components/VideoConverter.h`, `core/Factory.h`,
  `core/Context.h`, `core/Surface.h`. These are the authoritative names — read them before coding
  (APIs get hallucinated otherwise; see CLAUDE.md).
- `nvidia/encoder.rs` — the working ULL reference to mirror property-for-property (CBR, ~1-frame
  VBV, no B, slices, intra-refresh, on-demand IDR, dynamic bitrate, zero-copy DX11 input).
- `scaler.rs` — the D3D11 VideoProcessor to extend for BGRA→NV12 (+ scale) in one pass.
- `pipeline.rs` — `build_zero_copy` (shared keyed-mutex texture) and the latest-wins encode loop to
  reuse verbatim; the encoder is the only swapped part.
