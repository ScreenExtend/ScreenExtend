# AMD AMF encoder backend

> **North star: minimum glass-to-glass latency.** This backend gives Radeon and Ryzen-iGPU
> machines the same ultra-low-latency path `nvidia/encoder.rs` gives NVIDIA: D3D11-native,
> one frame in flight, one-frame VBV, no B-frames, no lookahead, no pre-analysis. Where a knob
> trades a millisecond for a little PSNR, we spend the PSNR.

This describes what is **built**, in `amd_sys/` (the FFI) and `amd/encoder.rs` (the encoder). It
replaces an earlier version of this file that was a pre-implementation spec; several of that
spec's property names did not exist and several of its values were backwards. Where this
document and `docs/research/amd-amf.md` disagree, the research report wins — it was verified
against the real headers and against OBS, FFmpeg and Sunshine.

**Status: `UNVERIFIED`.** No machine on this project has an AMD GPU. The FFI is fully asserted
by `test/amd_layout.rs` against numbers a C compiler measured from the real AMF headers, and the
no-runtime path is tested, but nothing here has ever encoded a frame.
`docs/manual-tests/amd-amf.md` is the script that validates it on a Radeon box in one sitting.

---

## 1. Binding strategy — no SDK at build time

Same philosophy as NVENC: no import library, no SDK install, no `LIBCLANG`.

- The AMF runtime ships **with the Radeon driver** as `amfrt64.dll`. `amd_sys::Amf::load()`
  opens it with `libloading` and resolves exactly two exported functions, `AMFInit` and
  `AMFQueryVersion`. Everything else is a vtable call.
- `dumpbin /imports target\release\ScreenExtend.exe | findstr /i amf` must print **nothing**. If
  `amfrt64` appears in the import table something linked instead of loading, and the binary will
  refuse to start on every machine without a Radeon driver. This is the Windows analogue of the
  macOS `nm -u` rule in `CLAUDE.md`.
- `AMFInit` is called with **`AMF_MAKE_FULL_VERSION(1, 4, 30, 0)`**, not the vendored headers'
  1.5.2.0. AMF's contract is that a newer runtime serves an older client, never the reverse, so
  asking high is the one way to lock out hardware that would otherwise work. `AMFQueryVersion`'s
  value is logged for support tickets and drives nothing. Never parse the DLL's `ProductVersion`
  resource string — a 2025 driver shipped it as `1.5.0.0;1.2;CL`.
- Optional features are gated by `HasProperty` and by `SetProperty`'s return code
  (`AMF_NOT_FOUND` / `AMF_NOT_SUPPORTED`), never by a version number.
- A missing `amfrt64.dll` — including `LoadLibrary` error 126, which is normal on minimal driver
  installs and boxes missing the VC runtime — is a clean `Err` that the `Auto` chain falls
  through, not something the user is told about.

### The C ABI

`amd_sys/vtbl.rs` has one `#[repr(C)]` struct per vtable in exact slot order. Slot order **is**
the ABI: calling slot 48 where you meant 49 is not a type error, it is a call through the wrong
function pointer. The traps, all encoded in the bindings:

- `AMFFactory` and `AMFTrace` are **not** `AMFInterface`s. `CreateContext` is slot 0, `TraceW` is
  slot 0, and the factory is a DLL-owned singleton that must never be `Release`d.
- `AMFSurface` has two observer pairs — slots 11/12 take an `AMFPropertyStorageObserver*`, slots
  31/32 an `AMFSurfaceObserver*`. The C++ header overloads both onto `AddObserver`.
- `amf_long` (the return of `Acquire`/`Release`) is **32-bit** on Windows.
- `AMFVariantStruct` is 24 bytes with the union at offset 8 and is passed to `SetProperty` **by
  value**; `AMFVariantInit`/`AMFVariantAssign*` are header-only and are reimplemented in Rust.
- `AMFPropertyInfo` is deliberately **not modelled**: it is 112 bytes in the C branch and 120 in
  C++ (virtual destructor), and `amfrt64.dll` is the C++ one. `GetPropertyInfo` is never called;
  `HasProperty` does the probing instead.
- Every AMF interface except `AMFFactory`, `AMFTrace` and `AMFPlane` starts with the 13
  `AMFPropertyStorage` slots, which is why one set of property helpers drives components,
  surfaces, buffers and caps alike.

Slots the backend never calls are still declared as `*const c_void`, because the **total slot
count** is the only assertion that catches a member omitted entirely.

---

## 2. Architecture

```
capture (BGRA ID3D11Texture2D, on the display's adapter)
   │
   │  optional prescale: scaler.rs VideoProcessorBlt  ──┐
   ▼                                                    ▼
ring[NUM_IN_FLIGHT] ID3D11Texture2D  ← CopyResource ────┘   (created on the capture device)
   │  context.Flush()
   ▼
AMFSurface  ← CreateSurfaceFromDX11Native (slot 49, ZERO COPY: it wraps, it does not copy)
   ▼
AMFComponent AMFVideoEncoderVCE_AVC   SubmitInput [22] → QueryOutput [23], one frame in flight
   ▼
AMFBuffer (Annex-B H.264) → Vec<u8> → the unchanged transport
```

- **Share the capture `ID3D11Device`.** `InitDX11(capture_device, AMF_DX11_1)` is what makes
  surface import a pointer wrap. Passing NULL would make AMF create its own device and turn every
  submit into a hidden cross-device copy. `ID3D10Multithread::SetMultithreadProtected(TRUE)` is
  set on it — AMF's own `DeviceDX11.cpp` does it unconditionally, and AMD's fix for flicker plus
  *growing* latency on a Desktop-Duplication device was "do not use
  `D3D11_CREATE_DEVICE_SINGLETHREADED`" plus an explicit `Flush()` after the copy, which the
  stage path does. (`D3D11_CREATE_DEVICE_VIDEO_SUPPORT` is **not** an AMF requirement.)
- **Same-adapter is the common AMD case and the fastest.** On a Radeon desktop or a Ryzen APU
  laptop, display, capture and VCN are all one adapter, so the cross-adapter keyed-mutex dance
  the NVENC path needs on a hybrid laptop disappears. `Backend::Amf` is that path;
  `Backend::AmfCpu` is the own-device CPU-upload fallback when the capture device is not AMD.
- **Texture ring.** `CreateSurfaceFromDX11Native` wraps rather than copies, so the texture must
  stay alive and unmodified until AMF is done with it — which is *not* when `SubmitInput`
  returns. One frame in flight answers "is AMF done" by the packet arriving; the ring
  (`NUM_IN_FLIGHT = 2`) means a surface whose release the driver defers by a frame can still
  never be overwritten mid-read. No `AMFSurfaceObserver` is needed.
- **`SHADER_RESOURCE` is mandatory** on the ring textures. A texture lacking it fails with
  `AMF_DIRECTX_FAILED` at the first `QueryOutput`, not at wrap time, and `Duplicate()` does not
  help.
- **Input format is not assumed.** `PREFERRED_INPUT_FORMAT` is BGRA, which is what an unscaled
  capture hands over with no blt at all; the encoder logs that AMF's internal colour converter is
  therefore active, because AMD state that packed-RGB input silently invokes a converter that
  competes with the foreground application. If the pipeline ever hands it a different format,
  `retune_input_format` rebuilds the ring and re-`Init`s once. `Scaler::with_format` can emit
  NV12 and the **CPU bridge** asks for it (`docs/OPTIMIZATIONS.md` rows 108 and 245: readback
  1192 → 740 µs at 1920x1080 → 1280x720); the GPU ring does not, because paying for a
  `VideoProcessorBlt` purely to convert has never been measured against AMF's own converter on
  AMD hardware.
- **Annex-B output.** AMF AVC emits start codes, and `ExtraData` is the Annex-B SPS+PPS, cached
  after `Init` for the out-of-band path.

---

## 3. Encoder settings — the latency table

Applied by `apply_properties`, in this order, all before `Init()`.

| # | Property | Value | Why |
|---|---|---|---|
| 1 | `L"Usage"` | `ULTRA_LOW_LATENCY` (1) | **First.** It re-defaults most other rate-control and GOP properties, so anything set before it is lost. It is also the biggest single lever: AMD's own doc says transcoding mode "needs to accept at least 3 input frames before any output is produced" while "in low latency modes output becomes available as soon as the first submitted frame is encoded" — 50 ms at 60 fps. Has a mandatory fallback; see section 4. |
| 2 | `L"QualityPreset"` | `SPEED` (1) | Fastest VCN path. **Codec-specific value**: SPEED is 1 on AVC, 10 on HEVC, 100 on AV1. |
| 3 | `L"FrameSize"` | `Size(w, h)` | Also passed to `Init`; both are set. |
| 4 | `L"FrameRate"` | `Rate(fps, 1)` | A wrong fps mis-sizes the VBV *and* the per-frame bit budget. |
| 5 | `L"Profile"` | `CONSTRAINED_BASELINE` / `MAIN` / `HIGH` | Matches the NVENC mapping. `ProfileLevel` is left at AMF's default: browsers are lenient about levels and pinning one low enough for every device would cap resolution. |
| 6 | `L"RateControlMethod"` | `CBR` (1), or `CONSTANT_QP` with `QPI`/`QPP` when `--qp` is set | Predictable per-frame size = predictable pacing. **AVC's CBR is 1**; on HEVC and AV1 the value 1 means `LATENCY_CONSTRAINED_VBR`. |
| 7 | `L"TargetBitrate"` / `L"PeakBitrate"` | both = target | Peak == target so no frame can balloon and head-of-line-block the next. |
| 8 | `L"VBVBufferSize"` | `bitrate / fps` | **The single biggest CBR latency lever** — one frame of bits. Sunshine's `rc_buffer_size = bitrate/framerate` and OBS's latency-VBR branch land on the same number independently. |
| 9 | `L"InitialVBVBufferFullness"` | **not set** (default 64 = 100%) | 0 means *empty*, which is the worst case for a rate controller that then has to fill it. |
| 10 | `L"EnforceHRD"` | `false` | Deliberately overrides ULL's `true`. HRD conformance obeys a buffer model our receiver is not. |
| 11 | `L"FillerDataEnable"` | `false` | Filler pads bytes to hit CBR exactly — wasted transmit time on our link. |
| 12 | `L"BPicturesPattern"` | `0` | **No B-frames.** A B-frame needs a future frame, hence ≥1 frame of reorder delay. Non-negotiable. |
| 13 | `L"MaxConsecutiveBPictures"` | `0`, only if `CAP_BFRAMES` | VCN 1.0–2.x (RDNA1 included) have no B-frames at all; setting it there returns `AMF_NOT_SUPPORTED`. |
| 14 | `L"LowLatencyInternal"` | `true`, optional | "enables low latency mode and POC mode 2". ULL sets it anyway; not on every driver. |
| 15 | `L"RateControlPreanalysisEnable"` | `PREENCODE_DISABLED` | Pre-encode is a first analysis pass = a frame of lookahead. (OBS sets this **on**.) |
| 16 | `L"EnablePreAnalysis"` | `false` | The full PA module is a lookahead pipeline. |
| 17 | `L"EnableVBAQ"` | `false` | Per-frame variance analysis for quality. |
| 18 | `L"HighMotionQualityBoostEnable"` | `false`, optional | Same. |
| 19 | `L"IDRPeriod"` | `i32::MAX` | Keyframes **on demand only** — join, PLI, FIR. Periodic IDRs are bitrate spikes, and a bitrate spike is a latency spike. Use a huge number, **not 0**: zero's meaning is undocumented for AVC, while Sunshine ships `gop_size = INT_MAX` and FFmpeg maps `gop_size` straight onto this property. |
| 20 | `L"IntraPeriod"` | `0` | No periodic I. |
| 21 | `L"HeaderInsertionSpacing"` | `0` | Do not spray SPS/PPS every N frames; in-band parameter sets ride the per-surface `InsertSPS`/`InsertPPS` on forced IDRs. (It is a spacing in frames, 0..=1000, **not** a mode — the mode enum exists on HEVC and AV1 only.) |
| 22 | `L"DeBlockingFilter"` | `true` | Free on the encoder, improves what the receiver shows. |
| 23 | `L"SlicesPerFrame"` + `L"OutputMode"` | `1`, `FRAME` | More slices only buy latency alongside `OUTPUT_MODE = SLICE`; without it AMF still returns one buffer per frame and the extra slices are pure compression cost. The NVENC path uses one slice too (`sliceMode = 0`). |
| 24 | `L"CABACEnable"` | `CABAC`, or `CALV` on Baseline | Matches NVENC, where Baseline forces CAVLC for a cheaper client-side decode. |
| 25 | `L"InputFullRangeColor"` + `L"FullRangeColor"` | `true` | We feed full-range colour and NVENC signals `videoFullRangeFlag = 1`; keeping them consistent stops AMD hosts looking washed out next to NVIDIA ones. `FullRangeColor` is the **only** colour property AMF actually writes into an AVC bitstream. |
| 26 | `L"OutColorPrimaries"` / `L"OutColorTransferChar"` / `L"OutMatrixCoeff"` / `L"OutColorProfile"` | BT.709, optional | Set in case a future driver honours them. AMF is reported never to write any of the four into an AVC bitstream (GPUOpen AMF issues #398, #277, #540, open on driver 24.8.1); only `FullRangeColor` reaches the stream. Since we packetize H.264 by hand, `amd/sps.rs` rewrites the SPS VUI instead — see section 3a. |
| 27 | `L"EnableEncoderSmartAccessVideo"` | `false`, optional | SmartAccess parallelises encode across VCN instances for *throughput*; splitting one low-latency stream adds coordination, not speed. |
| 28 | `L"InputQueueSize"` | `1`, optional | The default 16-deep queue is a latency reservoir. With one-in-flight it stays empty anyway, but a hard 1 makes `AMF_INPUT_FULL` a real signal instead of a 16-frame cushion. May be read-only. |
| 29 | `L"IntraRefreshMBsNumberPerSlot"` | **not set**, or `0` when `--intra-refresh off` | ULL already enables rolling intra-refresh at 255 MBs/slot ≈ a 32-frame wave at 1080p ≈ 0.53 s at 60 fps. Free loss resilience. |
| 30 | `L"MaxNumRefFrames"` | **never set** | Forcing 1 reference would silently *disable* intra-refresh — the docs make it conditional on `MaxOfReferenceFrames > 1` — and buys no latency, because with `B_PIC_PATTERN = 0` there is no reorder delay at any DPB size. |
| 31 | `L"MaxOfLTRFrames"` | `0` unless the caller asks | Mutually exclusive with intra-refresh; see section 5. |
| 32 | `L"QueryTimeout"` | `1` (ms), **read back** | Filed under "dynamic" in the header but effectively **static**: set after `Init` it returns `AMF_OK`, echoes back from `GetProperty`, and silently does nothing. Set before `Init` and honoured, `QueryOutput` blocks for us and we never sleep. |

**Per-frame**, on the input `AMFSurface`:

| Property | When |
|---|---|
| `L"ForcePictureType" = IDR` | join / PLI / FIR — the only keyframe trigger |
| `L"InsertSPS"`, `L"InsertPPS"` = `true` | on each forced IDR, so it is self-contained for a mid-stream joiner |
| `L"InsertAUD"` = `false` | always — access-unit delimiters are bytes no browser needs |
| `L"MarkCurrentWithLTRIndex"`, `L"ForceLTRReferenceBitfield"` | only when LTR is on |

Also per surface: `SetFrameType(PROGRESSIVE)`, `SetPts`, `SetDuration`, and **`SetCrop`** —
input hardware surfaces can be vertically aligned up by 16, and our virtual displays are
arbitrary client-driven sizes.

**Dynamic**, on the running component, with no `ReInit` and no `Flush`: `L"TargetBitrate"`,
`L"PeakBitrate"`, `L"VBVBufferSize"`. Dynamic writes take effect at the next `SubmitInput`. AMD
warn "you cannot change it too often", so `streamer::bitrate::BitrateController` stays the only
writer. Resolution never changes at runtime — that is a full re-init and an IDR spike.

**How optional properties are written.** Every knob that varies by driver or VCN generation goes
through `set_gated_*`, which probes `HasProperty`, writes, reads the value back, and records the
outcome; `apply_properties` prints all of them as one `AMF optional properties:` line, because on
a machine none of us can reach the log line is the diagnosis. `HasProperty` is *reported* rather
than obeyed — the write is attempted either way, since a refused optional write costs nothing on
AMF — so an entry reading `unknown-but-set:` is the runtime telling us what its `HasProperty`
actually means. Only the correctness core (`Usage`, `QualityPreset`, geometry, rate control,
`BPicturesPattern`, `IDRPeriod`) is fatal on failure.

---

## 3a. Colour signalling: we write the SPS VUI ourselves

AMF is reported never to write `OutColorProfile`, `OutColorPrimaries`, `OutColorTransferChar` or
`OutMatrixCoeff` into an AVC bitstream (GPUOpen AMF issues #398, #277 and #540, still open on
driver 24.8.1). Only `FullRangeColor` reaches the stream, and nothing writes
`bitstream_restriction_flag` at all — so a decoder infers the reorder depth from the level and
holds `min(16, MaxDpbMbs / PicSizeInMbs)` frames before showing the first, which is 33–267 ms at
60 fps for a stream that never reorders anything.

`amd/sps.rs` fixes it where we already own the bytes: `webrtc_session.rs` packetizes H.264 by
hand, so the SPS is ours. It finds the SPS NAL in the access unit and in `ExtraData`, unescapes,
copies the prefix **bit for bit** so nothing the encoder decided changes, writes a canonical VUI
(BT.709 primaries/transfer/matrix, `video_full_range_flag` matching the actual input format,
`bitstream_restriction_flag = 1`, `max_num_reorder_frames = 0`), re-escapes and splices it back.

Two properties of the design matter:

- **It is gated on a read-back**, not on a driver version. `read_extradata` parses the SPS AMF
  produced; if a future driver writes the colour description and the restriction correctly, the
  rewrite never runs. The per-frame cost is then a single NAL-header read, because the scan stops
  at the first slice NAL and parameter sets only ride forced IDRs (`HeaderInsertionSpacing = 0`).
- **The VUI is canonical, not merged.** Every field it could have to preserve is one this
  configuration deliberately turns off — `EnforceHRD = false`, no filler, no AUD, no SEI, one
  slice, no B-frames — and the transport is RTP, whose timestamps are the clock, so timing info
  goes too.

Verified by `windows_utils/streamer/test/h264.rs::parse_sps`, which was written separately from
the rewriter: a writer checked by its own parser proves nothing.

---

## 4. USAGE, and why it has a fallback

`USAGE = ULTRA_LOW_LATENCY` **breaks `Init()` outright** on pre-RDNA hardware from Adrenalin
23.7.1 onward. Two defences, in order:

1. AMD's own cheap probe: set `Usage`, then `GetProperty(L"RateControlPreanalysisEnable")`. If
   the read errors, the usage did not take → configure `LOW_LATENCY` instead.
2. Sunshine's blunt one: if `Init()` still fails under ULL, re-apply the **whole** property table
   (USAGE re-defaults it) under `LOW_LATENCY` and try again.

`LOW_LATENCY` differs from ULL only in rate control (PeakVBR), VBV size (4 Mb vs 735 kb) and
`LowLatencyInternal` — all of which the table overrides explicitly — so the fallback costs
almost nothing.

Afterwards, `Usage` and `RateControlMethod` are **read back and logged**. On GCN parts (RX 580,
Vega) `RateControlMethod` reportedly cannot be overridden at all after ULL: you keep ULL's
`LATENCY_CONSTRAINED_VBR` whatever you write. With a one-frame VBV that is a fine outcome;
silently believing you are in CBR when you are not is what makes bitrate control look broken.

What ULL sets for you (AMF encode doc, Annex A): `RateControl = LatencyConstrainedVBR`,
`VBV = 735 kb`, `EnforceHRD = true`, `LowLatencyInternal = true`, `IntraRefresh = 255 MBs/slot`,
`PreAnalysis = false`, `QualityPreset = SPEED`, `IDRPeriod = 240`, `QueryTimeout = 0`. Two of
those (`EnforceHRD`, `RateControlMethod`) we deliberately override; the rest we either keep or
re-assert.

---

## 5. Submission model

```
stage → Flush → wrap → SubmitInput → QueryOutput until non-NULL → release → repeat
```

Never submit frame N+1 before frame N's packet is out. Submitting ahead grows wall-clock latency
by (queue depth x frame time) while throughput looks perfect — the classic way an encoder
integration measures fine and feels awful in a stopwatch test.

- **"Nothing yet" is the NULL out-pointer, not a result code.** `QueryOutput` can return
  `AMF_OK` with a null `ppData`; AMD confirm `AMF_NEED_MORE_INPUT` is not a reliable "don't
  query" signal, and FFmpeg never tests either code.
- **`AMF_INPUT_FULL` means the surface was NOT consumed** — re-submit the *same* pointer, after
  draining output, because a full input queue means the output is not being drained. Under
  one-in-flight it should never fire; the code logs loudly once if it does, and concatenates any
  bytes harvested during the retry onto this frame's access unit rather than dropping them.
- **Do not sleep when `QueryTimeout` is honoured.** `QueryOutput` does the waiting. A `Sleep(1)`
  on Windows is really 1–15 ms depending on timer resolution.
- **One thread inside the encoder.** AMD's stated model is two (submit and query), but with
  strict one-in/one-out there is nothing for a second thread to overlap, and OBS ships
  single-threaded. Capture/encode overlap comes from outside instead — see section 5a.
- **Repeat** for a static desktop is literally "submit the staged texture again": zero copies,
  driven by `spawn_repeater` on the inline path and by the ring's own keepalive on the ring.
- **The access unit is copied into a retained `au_scratch`** while AMF still owns the mapping,
  and the owned `Vec` the pipeline wants is built after the buffer is released. Same shape and
  same reason as `nvidia/encoder.rs`.
- **An output object carrying zero bytes is a consumed frame, not "nothing yet".** `query_once`
  classifies the three cases explicitly (`classify_query_output`); folding an empty non-null
  buffer into the NULL-pointer case made it poll to `SUBMIT_DEADLINE` and then kill the session
  for a frame that had already been handed over.
- **Teardown** is `Drain` → `QueryOutput` until `AMF_EOF` → `Terminate` → `Release` on the
  component, then `Terminate` → `Release` on the context; the factory is DLL-owned and
  `AMFTrace` has no `Release` slot. `libloading::Library` is the **last** struct field so it
  drops last: unloading it while any AMF object is alive frees the code the vtables point into.
  `Flush()` is reserved for re-init, never for shutdown.
- **Errors** classify into transient (retry), recoverable (device-lost class: `AMF_NO_DEVICE`,
  `AMF_DIRECTX_FAILED`, `AMF_RESOLUTION_CHANGED` — tear down and rebuild) and fatal
  (`AMF_ENCODER_NOT_PRESENT`, `AMF_SURFACE_FORMAT_NOT_SUPPORTED`, `AMF_INVALID_RESOLUTION` — fall
  through to the next vendor). The transient message deliberately contains "device busy", which
  is the substring `pipeline::is_transient_encode_error` already looks for.

### Capabilities

`GetCaps` works **before** `Init()`, and the whole report is logged, because on a machine none of
us can reach the log line is the diagnosis: acceleration type, `MaxBitrate`, `NumOfStreams`,
`MaxProfile`, `MaxLevel`, `MaxReferenceFrames`, `NumOfHwInstances`, `BFrames`, `PreAnalysis`,
`QueryTimeoutSupport`, `SupportSliceOutput`, `ColorConversion`, throughput used/max, the input
width/height ranges, and whether BGRA and NV12 are in the input format list and `native`.
Construction refuses anything but `AMF_ACCEL_HARDWARE` or `AMF_ACCEL_GPU` — note that
`AMF_ACCEL_HARDWARE` is **0** and "not supported" is **-1**, so a truthiness test would be
backwards.

### LTR

`MaxOfLTRFrames` (0..=2, static) switches the encoder from ULL's free rolling intra-refresh to
user-controlled long-term references — the two are mutually exclusive ("With user control of LTR,
B-pictures and Intra-refresh features are not supported"), so this is a **choice between two
loss-recovery strategies**, not an addition.

It is on where something drives it and off where nothing does:

- **The ring path asks for it.** `build_amf_ring` requests `MAX_LTR_FRAMES_SUPPORTED`, because
  `ring_encode_thread` consumes `streamer::repair::RepairState` — the transport writes receipts,
  the encode thread turns them into `mark_ltr` / `use_ltr` at its next submit. A repair frame
  predicted from a picture the client is known to hold costs a fraction of an IDR and does not
  reset the rate controller.
- **The inline path passes 0.** Nothing there reads receipts, so turning LTR on would trade a
  free rolling refresh for a feature no one uses.
- **Both are gated on the capability, twice.** `open` requires `HasProperty(MaxOfLTRFrames)` and
  `caps.max_reference_frames > 1` (the docs make intra-refresh conditional on more than one
  reference, and an adapter that reports one cannot do user-controlled LTR at all), and
  `configure_and_init` then believes the property **read-back**, not the request. Believing we
  hold slots the driver never gave us would make every repair reference a picture that was never
  a long-term reference.
- **There is no `invalidate_reference`.** AMF has no `nvEncInvalidateRefFrames` analogue; it
  *disallows* LTRs outside the forced bitfield from current and future prediction instead, which
  reaches a similar end by a different mechanism. `RingEncoder::invalidate_reference` therefore
  reports `Unsupported` on AMF, which `ring_encode_thread` already reads as "stop the transport
  asking" — on AMF the repair *is* the forced bitfield.

`mark_next_frame_as_ltr` / `force_ltr_reference` set the per-surface properties; `last_marked_ltr`
and `last_referenced_ltr` read back what AMF reported on the output buffer.

---

## 5a. The async ring: where capture/encode overlap comes from

The encoder is one-in-flight and single-threaded by design. Overlap comes from
`pipeline.rs::AsyncEncodeRing`, which both hardware backends share:

```
WGC callback:   take a free slot → blt/CopyResource into it → Flush → send ReadyMsg   (returns)
encode thread:  ReadyMsg → repair receipts → SubmitInput → QueryOutput → broadcast → free the slot
```

Without it, every AMD frame was encoded *inside* the WGC `TypedEventHandler` — the blt, the
`Flush`, the wrap, the submit, the blocking `QueryOutput` and the broadcast, serially, on the
thread WGC is waiting on.

The two backends differ in exactly one place, `RingTextures`:

- **NVENC** (`Shared`) needs cross-adapter shared textures and a keyed mutex per slot, because
  its encoder lives on the dGPU while capture happens on the iGPU.
- **AMF** (`Local`) has neither: `new_on_device_with_ltr` builds the input textures on the
  **capture** device, which is what makes `CreateSurfaceFromDX11Native` a pointer wrap rather
  than a hidden cross-device copy. Nothing is shared, nothing is opened, and the free/ready
  channels are the whole ownership protocol — so `PoisonedSlots` and `SlotEncodeError::Poisoned`
  cannot fire on this path.

The post-stage `Flush()` stays on both: it is AMD's own fix for flicker *and growing latency* on a
Desktop-Duplication device (AMF issue #92), and removing it needs a measurement plus a torn-frame
soak, not a faster number.

Selection: the AMF ring is used for `--encoder amd`, and for `auto` **only when the capture device
is itself AMD** — which is exactly the condition `build_backend`'s Auto ordering already reasons
about ("an all-AMD box never reaches the Intel branch because its capture device is not Intel"),
so an NVIDIA or hybrid host is unaffected. If the ring cannot be built, the classic
`build_backend` path still produces `Backend::Amf` / `Backend::AmfCpu` as before.

---

## 6. Verifying it

Fully verifiable here, without an AMD GPU:

- `cargo test --lib windows_utils::streamer::test::amd_layout` — every struct size, field offset,
  vtable slot index, total slot count, enum value and property-name string, against numbers
  `cl.exe` measured from the vendored headers, plus the `QueryOutput` classification. Regenerate
  with `src-tauri/tools/amf_layout_build.bat`; see `src-tauri/tools/README.md`.
- `cargo test --lib windows_utils::streamer::amd::sps` — the SPS VUI rewriter, read back with the
  independently written parser in `test/h264.rs`.
- The graceful-absence path: with no `amfrt64.dll`, the loader returns a typed `Err` naming the
  DLL, `Encoder::new` fails cleanly, and `build_backend`'s `Auto` chain falls through.
- `dumpbin /imports … | findstr /i amf` prints nothing.

Everything else needs a Radeon desktop or a Ryzen APU laptop: `docs/manual-tests/amd-amf.md`.

---

## 7. Latency traps

- Setting `USAGE` after any other rate-control property — it re-defaults them.
- Letting AMF create its own D3D11 device instead of sharing the capture one.
- Submitting more than one surface before querying output.
- A VBV larger than ~1 frame, or filler data on.
- `MaxNumRefFrames = 1` — it disables intra-refresh and buys nothing.
- `SlicesPerFrame > 1` without `OUTPUT_MODE = SLICE` — compression cost, no latency gain.
- `QueryTimeout` set after `Init` — accepted, echoed back, ignored.
- `InitialVBVBufferFullness = 0` — that is 0%, i.e. empty.

## 8. Future work

- **NV12 input on the GPU ring.** The CPU bridge already takes NV12 (`docs/OPTIMIZATIONS.md` rows
  108 and 245); the ring does not, because on the GPU path there is no readback to halve and
  paying for a `VideoProcessorBlt` purely to convert has never been measured against AMF's
  internal converter on AMD hardware. That measurement is the open question.
- **Slice-mode output** (`OUTPUT_MODE = SLICE`, AMF 1.4.30+, gated on `SupportSliceOutput`) is
  the only genuinely sub-frame latency mechanism AMF offers, but it needs
  `webrtc_session.rs`'s hand-rolled H.264 packetizer to emit partial access units — and a
  slice-level pipeline that exists on only one vendor is not worth that complexity. Row 246
  rejected it on a measured ~2 ms ceiling on NVENC; the AMD number is the one nobody has.
- **HEVC / AV1.** Everything is name-mangled per codec (`AMFVideoEncoderHW_HEVC`,
  `AMFVideoEncoderHW_AV1` — note the macro names do not match the wire strings) and the enum
  values are renumbered, so each codec needs its own Rust enums and property module. Gate purely
  on `CreateComponent(<id>) == AMF_OK` plus `GetAccelerationType() == AMF_ACCEL_HARDWARE`, never
  on a version or a GPU name.

## 9. References

- `docs/research/amd-amf.md` — the verified report this implementation was written from: measured
  vtable slots, the per-USAGE default table, the OBS / FFmpeg / Sunshine comparison, and the
  corrections to this file's earlier draft.
- `src-tauri/resources/amf-headers/` — the vendored MIT headers (AMF 1.5.2.0), with provenance
  and SHA-256 in `resources/PROVENANCE.md`.
- `nvidia/encoder.rs` — the sibling ULL configuration; the two rationale blocks should stay in
  step.
- `scaler.rs` — the `ID3D11VideoProcessor` that will do BGRA→NV12 and scale in one pass.
- `pipeline.rs` — `build_backend`, the `Backend` enum and the latest-wins encode loop.
