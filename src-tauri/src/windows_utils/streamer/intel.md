# Intel Quick Sync (oneVPL) encoder backend — design & implementation guide

> **North-star: minimize glass-to-glass latency. Period.** This backend gives Intel-graphics
> machines the *same* ultra-low-latency path NVENC gives NVIDIA: **D3D11-native, zero-copy,
> `AsyncDepth=1` (one frame in flight), ~1-frame VBV, no B-frames, no lookahead.** Every setting
> below is picked for latency first. Intel has a structural advantage here — see §2.

It implements `crate::platform::EncoderBackend` for Intel GPUs via **oneVPL** (the successor to
Media SDK / "MFX") driving the **Quick Sync** fixed-function encoder. Treat this as the spec for
`intel.rs`; it is the Intel mirror of the rationale block atop `nvidia/encoder.rs`.

---

## 1. Binding strategy — no SDK at build time

Same philosophy as NVENC/AMF: **dynamic-load, no import lib, no toolkit.**

- The oneVPL **dispatcher** + Intel runtime ship with the **Intel graphics driver**
  (`libmfx*`/oneVPL runtime DLLs on the system path). We dynamically load the dispatcher
  (`libloading`) and resolve the C entry points: `MFXLoad`, `MFXCreateConfig`,
  `MFXSetConfigFilterProperty`, `MFXCreateSession`, and the `MFXVideo{CORE,ENCODE,VPP}_*` /
  `MFXMemory_*` functions. All flat C — no COM vtables (simpler than AMF).
- We vendor only the oneVPL **headers** (`vpl/*.h`, MIT) and hand-translate the structs/enums we
  touch (`mfxVideoParam`, `mfxFrameSurface1`, `mfxExtCodingOption{,2,3}`, `mfxEncodeCtrl`,
  `mfxBitstream`, the `MFX_*` enums) into an `intel_sys/` module, mirroring `nvidia/nvenc_sys/`.
  No `libmfx.lib`, no toolkit, no `LIBCLANG` at build time.
- **Require hardware + D3D11** at dispatch time via config filters: `mfxImplDescription.Impl =
  MFX_IMPL_TYPE_HARDWARE` and `AccelerationMode = MFX_ACCEL_MODE_VIA_D3D11`. If the dispatcher
  returns only software, **fail over to the next vendor** — a software encode is a latency
  non-starter and must never be selected silently.

---

## 2. Architecture (Intel's structural latency win)

```
WGC capture (BGRA, ID3D11Texture2D, on the Intel iGPU that scans out the display)
   │  *** SAME ADAPTER ***  capture, VPP, and QSV encode are all on the iGPU
   ▼  zero copy — no cross-adapter bounce
MFX VPP  (BGRA/RGB4 → NV12, + optional downscale)   LowPower scaling, one GPU pass
   ▼
mfxFrameSurface1 backed by the NV12 ID3D11Texture2D (video memory, zero copy)
   ▼
MFXVideoENCODE_EncodeFrameAsync  (LowPower/VDEnc, AsyncDepth=1)  → syncp
   ▼
MFXVideoCORE_SyncOperation  → mfxBitstream (Annex-B H.264) → Bytes → broadcast (unchanged transport)
```

- **The whole project's hybrid-GPU headache is usually gone here.** On the typical laptop the
  Intel iGPU **owns the attached display**, so capture + VPP + QSV encode are all on **one
  adapter**. The cross-adapter copy the NVENC path needs on this machine **does not exist** — this
  can make the Intel path the *lowest-latency capture→encode* path on a hybrid box even though the
  encoder is "weaker," because zero cross-adapter traffic beats a fast encoder reached over a copy.
- **Share the capture device:** `MFXVideoCORE_SetHandle(session, MFX_HANDLE_D3D11_DEVICE, device)`
  with the *same* `ID3D11Device` that owns the captured texture. Never let oneVPL create its own
  device.
- **`LowPower = ON` (VDEnc).** Select the fixed-function **VDEnc** path, not the PAK+ENC shader
  path. VDEnc is the low-latency, low-power encoder and is the one tuned for display-remoting; it
  supports the low-latency features below. This is an Intel-specific must-set.
- **Color convert via MFX VPP** (BGRA→NV12, + scale) as one GPU pass feeding video-memory NV12
  surfaces the encoder reads in place. Alternatively reuse `scaler.rs` (D3D11 VideoProcessor) to
  emit NV12 and import those textures — pick one GPU pass, never a CPU convert.
- **Zero-copy surfaces:** use `IOPattern = MFX_IOPATTERN_IN_VIDEO_MEMORY` with D3D11-backed
  `mfxFrameSurface1`s (oneVPL 2.x surface-sharing / a `mfxFrameAllocator` wrapping the
  WGC/VPP textures). No system-memory surfaces on the hot path — that's a readback.
- **`AsyncDepth = 1` — one frame in flight.** This is the single most important Intel latency
  knob. The default (≈4) pipelines several frames for throughput and adds frames of wall-clock
  delay. Depth 1 = submit one, sync it, send it — the same latest-wins discipline as NVENC.
- **Annex-B output**, SPS/PPS in-band on every IDR (see settings) so joiners / post-PLI IDRs are
  self-contained.

---

## 3. Encoder settings — the latency table

Core `mfxVideoParam.mfx` + `FrameInfo` and three ext-buffers (`mfxExtCodingOption{,2,3}`). Names
are real oneVPL fields. The "why" is always latency.

### Core (`mfxVideoParam`)

| Field | Value | Why (latency) |
|---|---|---|
| `AsyncDepth` | `1` | One frame in flight. The default deep pipeline trades frames of latency for throughput we don't need. |
| `mfx.LowPower` | `MFX_CODINGOPTION_ON` | Select **VDEnc** fixed-function path — the low-latency, display-remoting-tuned encoder. |
| `mfx.CodecId` | `MFX_CODEC_AVC` | H.264, matching the negotiated track. |
| `mfx.TargetUsage` | `MFX_TARGETUSAGE_BEST_SPEED` (TU7) | Fastest encode = least encode time. We buy latency with quality; CBR keeps it clean. |
| `mfx.RateControlMethod` | `MFX_RATECONTROL_CBR` | Constant bitrate → predictable per-frame size → predictable pacing. No VBR ballooning. |
| `mfx.GopRefDist` | `1` | **No B-frames.** Distance 1 = I/P only → zero reorder delay. Forbidden to raise. |
| `mfx.GopPicSize` | very large (≈ infinite) | No periodic IDR; keyframes on demand only (join/PLI/FIR). Scheduled IDRs are bitrate/latency spikes. |
| `mfx.IdrInterval` | `0` | Every keyframe is an IDR (clean recovery point), but they're still on-demand via GopPicSize. |
| `mfx.NumSlice` | `4`–`8` | Slice-based output: a lost packet corrupts one slice region, not the frame, and slices packetize as they finish. Matches NVENC. |
| `mfx.TargetKbps` / `mfx.MaxKbps` | equal | Max == target so no frame exceeds budget and HOL-blocks the next. |
| `mfx.BufferSizeInKB` | ≈ **1 frame** of bits | ~1-frame VBV — the biggest CBR latency lever; mirrors NVENC's ~1-frame VBV. |
| `mfx.InitialDelayInKB` | `0` | No initial buffering delay before the first frame leaves. |
| `mfx.CodecProfile` | `MFX_PROFILE_AVC_CONSTRAINED_BASELINE` (or `MAIN`) | Baseline avoids CABAC → cheaper, more pipelineable **decode** on the receiver. Match NVENC default. |
| `mfx.FrameInfo.FourCC` / `ChromaFormat` | `MFX_FOURCC_NV12` / `YUV420` | Encoder's native input; VPP produces it. |
| `mfx.FrameInfo.FrameRateExtN/D` | actual fps | Correct fps so CBR sizes per-frame bits right; wrong fps mis-sizes the VBV. |
| `mfx.FrameInfo.PicStruct` | `MFX_PICSTRUCT_PROGRESSIVE` | Progressive only — no interlace handling. |
| `IOPattern` | `MFX_IOPATTERN_IN_VIDEO_MEMORY` | D3D11 video-memory input = zero copy. System memory = a readback. |

### `mfxExtCodingOption` (CO)

| Field | Value | Why |
|---|---|---|
| `MaxDecFrameBuffering` | `1` | Tells the decoder it needs to buffer only 1 frame → minimal receiver-side DPB delay. |
| `NalHrdConformance` | `MFX_CODINGOPTION_OFF` | Drop strict HRD timing so the RC isn't forced to insert conformance delay. |
| `PicTimingSEI` | `MFX_CODINGOPTION_OFF` | Skip timing SEI overhead. |
| `AUDelimiter` | `MFX_CODINGOPTION_OFF` | Skip AU delimiters — bytes we don't need. |

### `mfxExtCodingOption2` (CO2)

| Field | Value | Why |
|---|---|---|
| `IntRefType` | `MFX_REFRESH_HORIZONTAL` (or vertical) | **Rolling intra-refresh** — heals loss passively with no full-IDR spike (the project's latency-favoring recovery; mirrors NVENC `intra_refresh`). |
| `IntRefCycleSize` | sized for a **~1 s** refresh wave | Spread the refresh so no single frame spikes in size. |
| `IntRefQPDelta` | small (e.g. `0`–`2`) | Keep refreshed MBs from spiking bits. |
| `BRefType` | `MFX_B_REF_OFF` | Belt-and-suspenders: no B-pyramid (we already have GopRefDist=1). |
| `AdaptiveI` | `MFX_CODINGOPTION_OFF` | No encoder-decided extra IDRs (we control keyframes for join/PLI only). |
| `RepeatPPS` | `MFX_CODINGOPTION_ON` | Resend SPS/PPS so any IDR is self-decodable for a joiner / post-loss. |
| `MaxSliceSize` | optional, ≈ MTU-friendly | Cap slice **bytes** so each slice fits transport packetization — tighter loss localization. (Mutually exclusive with `NumSlice` on some drivers; pick one.) |

### `mfxExtCodingOption3` (CO3)

| Field | Value | Why |
|---|---|---|
| `ScenarioInfo` | `MFX_SCENARIO_DISPLAY_REMOTING` (or `REMOTE_GAMING`) | **Intel's purpose-built low-latency tuning** for exactly our use case — biases the RC/encoder for instant display streaming. Set it. |
| `LowDelayBRC` | `MFX_CODINGOPTION_ON` | Low-delay CBR: tightly caps per-frame size variation so a complex frame can't balloon and stall the link. Critical for a smooth tail. |
| `WinBRCSize` | `1` | 1-frame sliding-window rate control → no multi-frame bit banking. |
| `WinBRCMaxAvgKbps` | = target | Window cap == target so the window can't overshoot. |
| `GPB` | `MFX_CODINGOPTION_OFF` | No generalized B-pred (VDEnc); P-only path. |

**Per-frame (`mfxEncodeCtrl` passed to `EncodeFrameAsync`):**

| Field | When | Why |
|---|---|---|
| `FrameType = MFX_FRAMETYPE_I \| MFX_FRAMETYPE_IDR \| MFX_FRAMETYPE_REF` | join / inbound PLI / FIR | On-demand keyframe — the only keyframe trigger; wire to the pipeline's `idr_request`. |

---

## 4. Dynamic controls (no teardown)

- **Adaptive bitrate** (`set_bitrate`): for CBR, update `TargetKbps`/`MaxKbps` and call
  **`MFXVideoENCODE_Reset`** with the new param — oneVPL's Reset is a *fast* in-place
  reconfiguration (not a full rebuild) and does **not** require new surfaces as long as resolution
  is unchanged. Drive it from the existing TWCC-ish controller via `Pipeline::set_target_bitrate`.
  `LowDelayBRC` makes the change take effect within a frame or two.
- **Force IDR**: per-frame `mfxEncodeCtrl.FrameType` above; same semantics as NVENC `force_idr()`.
- **Never change resolution at runtime.** VPP + encoder surfaces are pinned at init (like NVENC);
  a resize is a teardown + IDR spike = exactly the latency we remove. Operator picks `--scale` at
  startup.

---

## 5. Milestones (gate one at a time; each must compile, run, and pass before the next)

Mirror the project's M-gating; probes parallel the NVENC ones, selected onto the Intel backend.

- **M-INT0 — dispatch & device.** `MFXLoad` → require HW + D3D11 via config filters →
  `MFXCreateSession` → `SetHandle(D3D11_DEVICE)` on a throwaway device → query the AVC encoder
  → close. **Test:** logs the selected **hardware** impl + adapter and the runtime API version;
  refuses (and would fail over) if only software is available. Loop 100× — no leaks.
- **M-INT1 — VPP NV12.** Init MFX VPP BGRA→NV12 (+ optional scale); convert a known BGRA texture
  and dump NV12. **Test:** `ffplay -f rawvideo -pix_fmt nv12 -s WxH out.nv12` shows the right
  image; checksum matches for the synthetic pattern.
- **M-INT2 — encode to file (low-latency).** Encode 300 synthetic NV12 frames with the §3 settings
  to `out_intel.h264` (`LowPower=ON`, `AsyncDepth=1`, CBR, GopRefDist=1). **Test:** `ffprobe`
  confirms **AVC, `has_b_frames=0`**, SPS/PPS present, slice count 4–8; `ffplay` plays it; logged
  encode-time/frame is in a sane low-latency band.
- **M-INT3 — zero-copy video memory.** Switch to `IOPATTERN_IN_VIDEO_MEMORY` with D3D11-backed
  surfaces (no system-memory copy); assert **no readback** on the hot path. **Test:** a debug
  counter shows zero `Map`/`memcpy`/`CopySubresource` to system memory per frame; encode-time/frame
  drops vs M-INT2; GPUView shows no CPU-GPU bounce.
- **M-INT4 — same-adapter live.** Wire into the live pipeline where the Intel iGPU owns the display:
  WGC → VPP(NV12) → QSV → track. **Test:** `--probe-live` writes 150 clean frames; the WHEP
  self-test asserts RTP flows; on-screen latency looks instant. (Expect this to be the *lowest*
  capture→encode latency on a hybrid box — no cross-adapter copy.)
- **M-INT5 — cross-adapter fallback.** Rare (Intel iGPU encodes, but a *different* adapter owns the
  captured display): reuse `pipeline.rs::build_zero_copy`'s keyed-mutex shared texture; log the
  chosen path. **Test:** force it, confirm 150 clean frames and a single GPU→GPU copy.
- **M-INT6 — dynamic bitrate + IDR.** Live `Reset`-based bitrate steps and forced IDR on PLI.
  **Test:** an Intel analog of `--probe-bitrate` steps target down/up; byte-rate tracking shows the
  stream following within a few frames **with no resolution change and no IDR storm**; an injected
  PLI yields exactly one IDR.

---

## 6. How to test latency (the only metric that matters)

- **Stream sanity:** `ffprobe -show_frames out_intel.h264` → only I/P (never B), `has_b_frames=0`,
  SPS/PPS reappear at each IDR.
- **Encode-path latency:** reuse the pipeline's per-60-frame avg/max encode-time log; Intel should
  sit in a low-latency band (fixed-function VDEnc is fast); watch the **max/tail** over 5+ minutes,
  not just the average — the tail is where a VBV/BRC regression shows up.
- **Glass-to-glass:** the §16 photographic stopwatch method, AMD/NVENC/Intel compared on the same
  box. Confirm hardware decode in `chrome://media-internals` and the candidate pair / no-relay in
  `chrome://webrtc-internals`.
- **Same-adapter proof:** log the capture display's adapter LUID vs the encode adapter LUID; on the
  common laptop they match → assert the zero-copy same-adapter path was taken (no shared-texture
  copy).

---

## 7. Latency traps to avoid (oneVPL-specific)

- Leaving `AsyncDepth` at its default → several frames of pipeline latency for throughput we never
  use. **Always 1.**
- `LowPower = OFF` → the PAK+ENC shader path instead of VDEnc; higher latency and missing some
  low-latency tuning. **Always ON** for this use case.
- System-memory `IOPattern` or a `mfxFrameAllocator` that lands in system memory → a per-frame
  readback (the classic QSV latency pitfall the project explicitly avoids elsewhere).
- Forgetting `LowDelayBRC`/`WinBRCSize=1` → CBR banks bits across frames; a busy frame balloons and
  stalls the link (a tail-latency spike).
- Not setting `ScenarioInfo = DISPLAY_REMOTING` → you leave Intel's purpose-built low-latency tuning
  on the table.
- Software fallback selected silently because the HW filter wasn't required at dispatch → orders of
  magnitude worse latency. Require HW; fail over instead.
- A deep `mfxBitstream` reuse / not syncing each frame promptly → output buffered behind the async
  pipeline.

---

## 8. References
- Vendored **oneVPL headers** (`vpl/mfxstructures.h`, `mfxvideo.h`, `mfxenc.h`,
  `mfxencodecstat.h`, `mfximplcaps.h`) — authoritative for every `mfx*` field and `MFX_*` enum
  (`AsyncDepth`, `LowPower`, `mfxExtCodingOption{,2,3}`, `ScenarioInfo`, `LowDelayBRC`,
  `IntRefType`, …). Read them before coding — APIs get hallucinated otherwise (CLAUDE.md).
- oneVPL **low-latency / display-remoting** guidance (the `MFX_SCENARIO_DISPLAY_REMOTING` +
  `AsyncDepth=1` + `LowPower` recipe) — the canonical Intel low-latency configuration.
- `nvidia/encoder.rs` — the working ULL reference to mirror setting-for-setting (CBR, ~1-frame VBV,
  no B, slices, intra-refresh, on-demand IDR, dynamic bitrate, zero-copy DX11 input).
- `scaler.rs` — the D3D11 VideoProcessor; either reuse it for BGRA→NV12 (+scale) or mirror it with
  MFX VPP. One GPU pass, never a CPU convert.
- `pipeline.rs` — `build_zero_copy` and the latest-wins encode loop to reuse; only the encoder
  swaps.
