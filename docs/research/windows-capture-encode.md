# Windows capture → scale → encode: how fast can it get, and how do we probe for it?

Research report for the ScreenExtend latency & compatibility overhaul.
Companion discovery reports: `docs/discovery/F-windows-capture-fork.md` (the vendored
`windows-capture` fork, diffed against upstream 2.0.0). `D-windows-streamer-core.md`,
`E1-nvenc.md` and `E2-intel-x264-amd.md` did not exist when this was written; the ~40
`findings/` files covering those files did, and this report builds on them rather than
restating them.

---

## 1. Question

How do we make the Windows capture → scale → encode path as fast as it can be on every host
from Windows 10 (2020-era, build 19041/2004) through Windows 11 26xxx, with every capability
selected by **runtime probing** rather than by OS version number?

Sub-questions, per the brief: WGC configuration and feature probing; DXGI Desktop Duplication
frame metadata and dirty rects; NVENC's latency-optimal configuration including sub-frame
readback, async output and LTR; Quick Sync / oneVPL low-delay settings and the cross-adapter
problem; x264 zerolatency and slice-level output; D3D11/OS scheduling knobs; and robust
adapter selection on hybrid-graphics laptops.

---

## 2. Findings

### 2.1 Windows Graphics Capture (WGC)

**Feature-by-feature availability.** The version table below is from the Microsoft Learn
moniker ranges on each property page — this is the authoritative source, and it corrects the
brief's assumptions in two places.

| Property | First SDK / build | Notes |
|---|---|---|
| `GraphicsCaptureSession` itself | 10.0.17134 (1803), UniversalApiContract v6 | `IsSupported()` is a separate runtime check |
| `IsCursorCaptureEnabled` | 10.0.19041 (2004) | listed in the class's own "Version history" table |
| `IsBorderRequired` | 10.0.20348 (Server 2022 / 21H1 SDK) | **setting it to `false` additionally requires `GraphicsCaptureAccess.RequestAccessAsync(GraphicsCaptureAccessKind.Borderless)` and the `graphicsCaptureWithoutBorder` capability**; without consent the setter *succeeds and is ignored* |
| `IncludeSecondaryWindows` | ~10.0.22000 (Win11 21H2) | window capture only; irrelevant to monitor capture |
| `MinUpdateInterval` | **10.0.26100 (Win11 24H2)** — monikers are `winrt-26100`, `winrt-insider`, `winrt-28000` only | **not 22H2**, contrary to the brief |
| `DirtyRegionMode` | **10.0.26100 (Win11 24H2)** — same moniker set | `GraphicsCaptureDirtyRegionMode.ReportOnly` / `ReportAndRender` |
| `ConfigurationIteration` | 26100+ | not useful here |

Sources: [GraphicsCaptureSession class](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession),
[MinUpdateInterval](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.minupdateinterval),
[DirtyRegionMode](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.dirtyregionmode),
[IsBorderRequired](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired),
[GraphicsCaptureAccess.RequestAccessAsync](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscaptureaccess.requestaccessasync).

**The probe is `ApiInformation.IsPropertyPresent`**, per-property, and the correct response to
a `false` is to *skip that one setter*, not to fail the session. Both OBS and Sunshine do
exactly this (§3).

**`Create` vs `CreateFreeThreaded`.** `Create` requires a `DispatcherQueue` on the calling
thread and raises `FrameArrived` on that thread — which means the thread must run a message
loop, so each frame costs a queue enqueue plus a thread wake plus `GetMessageW` return plus
`DispatchMessageW`. `CreateFreeThreaded` has no `DispatcherQueue` requirement and raises
`FrameArrived` "on the frame pool's internal thread"; the callback must be agile
([Create](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.direct3d11captureframepool.create),
[CreateFreeThreaded](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.direct3d11captureframepool.createfreethreaded)).
Nobody publishes a measured delta, but it is one fewer scheduler-mediated hop per frame and
Sunshine's WGC backend uses `CreateFreeThreaded` while OBS uses `Create`.

**`numberOfBuffers`.** The docs say only "the number of buffers in which to store captured
frames". Both reference implementations use **2** (OBS `Create(..., 2, size)`, Sunshine
`CreateFreeThreaded(..., 2, item.Size())`). The pool is a queue: `TryGetNextFrame()` dequeues
the **oldest** buffered frame, so N buffers is N-1 frames of potential queue latency unless
the consumer drains. Sunshine drains implicitly — its `on_frame_arrived` closes the previously
produced frame and replaces it, so a slow consumer always sees the newest.

**`DirectXPixelFormat`.** Both OBS and Sunshine pick `B8G8R8A8UIntNormalized` for SDR and
`R16G16B16A16Float` for HDR. BGRA8 is the right choice for us and is what NVENC's
`NV_ENC_BUFFER_FORMAT_ARGB` expects (see §2.3).

**Frame cadence and `MinUpdateInterval` semantics.** WGC's default delivery is capped near
60 Hz irrespective of the display's refresh rate; `MinUpdateInterval` is the knob that raises
that ceiling, not a throttle. Sunshine's commit
["fix(win): Windows.Graphics.Capture API frame rate capped at 60fps"](https://github.com/LizardByte/Sunshine/actions/runs/19398755345)
sets a flat `4ms` (250 Hz) when the property is present. The `windows-capture` crate's issue
[#190](https://github.com/NiiightmareXD/windows-capture/issues/190) has the only measured
numbers I found: with the interval set to 1/60 s the actual capture rate was ~45 fps on a
60 Hz panel and ~52 fps on a 120 Hz panel; 1/90 gave ~58 fps and 1/120 gave ~59 fps on the
120 Hz panel. So the interval behaves as a *floor on the gap between frames* and must be set
comfortably **below** the target period, not equal to it.

**WGC vs DXGI Desktop Duplication latency.** There is no rigorous public measurement. What
exists is consistent qualitative reporting: DXGI DD gives direct framebuffer access and is
usually described as the lower-latency, full-display path, while WGC rides the DWM compositor,
survives overlays and mode changes, and is the only path that reliably works on multi-adapter
and protected-content configurations. The Sunshine/Lossless-Scaling communities report WGC as
*perceptually* lower-latency in practice because DXGI DD contends with overlays for the
framebuffer ([Sunshine capture-method docs](https://deepwiki.com/qiin2333/foundation-sunshine/4.1.1-windows-capture-methods),
[Lossless Scaling FAQ](https://sageinfinity.github.io/docs/FAQ/dxgiwgc)). **Treat "WGC is
faster" as unproven and measure it on the dev box** — the two paths already both exist here.

### 2.2 DXGI Desktop Duplication

`DXGI_OUTDUPL_FRAME_INFO`, verbatim from
[the reference](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/ns-dxgi1_2-dxgi_outdupl_frame_info):

- `LastPresentTime` — QPC timestamp of the last desktop-image update. **Zero means the desktop
  image was not updated since the last `AcquireNextFrame`.** This is the change flag.
- `LastMouseUpdateTime` — QPC timestamp of the last mouse position *or* shape change. Non-zero
  always means position is valid; `PointerShapeBufferSize != 0` additionally means the shape
  changed. "If only the pointer was updated, the `AccumulatedFrames`, `TotalMetadataBufferSize`
  and `LastPresentTime` members are set to zero."
- `AccumulatedFrames` — "the number of frames the OS accumulated in the desktop image surface
  since the calling application processed the last desktop image". **1 = we kept up; >1 = we
  fell behind and the OS coalesced update regions.** This is the free "am I dropping frames"
  telemetry.
- `RectsCoalesced` — TRUE if the OS accumulated updates by coalescing dirty regions, so "the
  dirty regions might contain unmodified pixels". A dirty-rect optimisation must treat this as
  "the rects are a superset".
- `ProtectedContentMaskedOut` — TRUE if protected content was already blacked out. **The
  application can use this to notify the remote user.** We currently never read it, so a user
  streaming DRM video sees a black rectangle with no explanation.
- `TotalMetadataBufferSize` — total bytes needed for move rects + dirty rects; you cannot
  query the two separately.

`AcquireNextFrame(timeout)` is a **wait**, not a poll: it returns as soon as a frame is
available, or `DXGI_ERROR_WAIT_TIMEOUT` when the timeout expires. So the timeout value costs
wakeups, not latency. The Microsoft `DXGIDesktopDuplication` sample uses 500 ms. The docs'
performance guidance is to "release the frame just before calling `AcquireNextFrame`… minimize
the time between releasing the current frame and acquiring the next", because the OS coalesces
updates while you hold a frame ([Desktop Duplication API](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api)).

`DuplicateOutput1` adds a supported-format list and is the path required for HDR/wide-gamut
duplication. It has **no latency benefit** for a BGRA8 SDR pipeline and it requires the
process be per-monitor DPI aware (we are). Not recommended for this overhaul.

**Dirty rects and partial encoding.** I searched for anyone driving an H.264 encoder from
duplication dirty rects and found none in the open-source streaming space. NVENC's
[emphasis map / `qpDeltaMap`](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html)
(`NV_ENC_RC_PARAMS::qpMapMode = NV_ENC_QP_MAP_EMPHASIS`, then a signed byte per macroblock in
`NV_ENC_PIC_PARAMS::qpDeltaMap`) exists and is documented for exactly the "client has a priori
knowledge of image complexity" case, and NVIDIA's own docs cite NVFBC classification as the
source. But it changes **quality**, not **cost**: the encoder still runs over the whole frame,
so it buys bitrate, not latency. **The technique that actually pays is the one already
implemented: skip the copy+encode entirely when nothing changed, and emit a repeat/keepalive
frame instead.** That is what `Duplicator::present_region_is_empty` does today. The only
unclaimed win is doing the same thing on the WGC path.

### 2.3 NVENC (SDK 12.1 bindings in-tree; dev box RTX 3080 Ti Laptop / Ampere, driver 580.97)

The [NVENC 13.0 programming guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html)
prescribes, for "ultra-low latency, with CBR":

- `presetGUID` P1–P4 (P1 = highest performance), `tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`.
- `rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR`, `averageBitRate` = target.
- `rcParams.enableLookahead = 0` (default); enabling it makes `NvEncEncodePicture` return
  `NV_ENC_ERR_NEED_MORE_INPUT` until enough input frames are buffered — a hard latency floor.
- `rcParams.lowDelayKeyFrameScale` — "control the I-to-P frame ratio and **avoid channel
  congestion in case I frame ends up generating high number of bits**". This is the documented
  antidote to a one-frame VBV plus an IDR.
- `NVENC_INFINITE_GOPLENGTH` + `enableIntraRefresh` for error resilience without a feedback
  channel; intra refresh is "applied in encode order and only on frames which can be used as
  reference".
- **Async mode**: "Asynchronous mode encoding is supported only on Windows 10 and above" with
  WDDM drivers; set `enableEncodeAsync = 1`, register a completion event per output buffer, and
  "the main encoder thread should be used only for encoder initialization and to submit work to
  the HW Encoder using `NvEncEncodePicture`, which is non-blocking". Avoid blocking
  `nvEncLockBitstream` on the submitting thread.
- **`enableSubFrameWrite = 1`**: "the hardware encoder returns data as soon as a slice
  (H264/HEVC) or tile (AV1) has completed encoding. This results in better encoding latency,
  but the application has to keep polling via `nvEncLockBitstream` continuously to see if any
  encoded slice/tile data is available… use if the marginal reduction in latency from sub-frame
  encoding is worth the increase in complexity due to CPU-based polling."
- `reportSliceOffsets` fills `NV_ENC_LOCK_BITSTREAM::sliceOffsets`, and **requires
  `enableEncodeAsync = 0`** and does not work with MB-based/byte-based slicing on Kepler. So
  `reportSliceOffsets` and async are mutually exclusive; sub-frame write is the async-compatible
  route.
- `enableOutputInVidmem = 1` "avoids PCIe transfers" — but only if the bitstream is consumed on
  the GPU. We packetize on the CPU, so this is a pessimisation for us.
- **Split-frame encoding** (`NV_ENC_INITIALIZE_PARAMS::splitEncodeMode`, SDK 12.1+): presets
  P1–P4 enable it implicitly when the GPU has >1 NVENC engine and the frame is tall enough.
  The [arXiv SFE evaluation](https://arxiv.org/html/2511.18687v1) measures 81.6–96.4 %
  throughput improvement at 4K and 85.7–96.4 % at 8K on a two-engine GPU, for an average
  0.04 dB PSNR / ~0.07 VMAF cost at 4K; latency is unchanged at 4K and drops by one frame
  (~16.7 ms) at 8K. **Ampere laptop parts have a single NVENC, so there is nothing to split on
  the dev box** — probe `NV_ENC_CAPS_NUM_ENCODER_ENGINES` (49).
- **LTR / reference-frame invalidation**: `enableLTR = 1` with `ltrNumFrames`; two modes — "LTR
  Trust" (`ltrTrustMode = 1`, auto-marks the first `ltrNumFrames` after IDR) which the guide
  says is "strongly discouraged as it may be deprecated", and "LTR Per Picture" where the client
  sets `NV_ENC_PIC_PARAMS_H264::ltrMarkFrameIdx` and selects references with `ltrUseFrameBitmap`.
  Separately, `NvEncInvalidateRefFrames` drops a corrupted reference; the encoder then falls back
  to older short-term/LTR frames and only emits an intra frame if nothing is left. The guide's
  advice for loss-resilient low-latency streaming is a **large DPB** so older references survive
  an invalidation.
- **CABAC vs CAVLC**: gate on `NV_ENC_CAPS_SUPPORT_CABAC` (7). CABAC costs the *decoder* more
  than the encoder on modern silicon; the reason to keep CAVLC is Baseline-profile clients.
- **Caps to query** (all present in `nvenc_sys/nvEncodeAPI.rs:1387-1444`, none called anywhere
  in the tree): `SUPPORTED_RATECONTROL_MODES` (1), `SUPPORT_CABAC` (7), `SUPPORT_SUBFRAME_READBACK`
  (23), `SUPPORT_CONSTRAINED_ENCODING` (24), `SUPPORT_INTRA_REFRESH` (25),
  `SUPPORT_DYNAMIC_SLICE_MODE` (27), `SUPPORT_REF_PIC_INVALIDATION` (28), `ASYNC_ENCODE_SUPPORT`
  (30), `NUM_MAX_LTR_FRAMES` (40), `SUPPORT_WEIGHTED_PREDICTION` (41), `SUPPORT_EMPHASIS_LEVEL_MAP`
  (44), `SUPPORT_MULTIPLE_REF_FRAMES` (47), `NUM_ENCODER_ENGINES` (49),
  `SINGLE_SLICE_INTRA_REFRESH` (50).
- **Session limits**: NVIDIA raised the consumer-GPU concurrent NVENC session cap from 5 to **8
  per system**, requiring **Windows driver 551.76+** (Linux 550.54.14+)
  ([VideoCardz](https://videocardz.com/newz/nvdia-geforce-gpus-now-support-up-to-8-concurrent-nvenc-encoding-sessions),
  [Tom's Hardware](https://www.tomshardware.com/news/nvidia-increases-concurrent-nvenc-sessions-on-consumer-gpus)).
  The dev box's 580.97 is well past that. **8 is still a hard ceiling on simultaneous clients on
  the NVENC path** — and ScreenExtend deliberately does not cap simultaneous displays, so the
  9th client must degrade to Quick Sync or x264 rather than fail.
- **Input format**: `NV_ENC_BUFFER_FORMAT_ARGB` is "8-bit packed A8R8G8B8", which in
  little-endian memory is B,G,R,A — i.e. it is exactly a `DXGI_FORMAT_B8G8R8A8_UNORM` texture.
  NVENC converts ARGB→NV12 internally on the way in; the conversion is done by the encoder's own
  fixed-function/CSC path, so feeding BGRA directly is *cheaper* than doing a VideoProcessor
  BGRA→NV12 pass ourselves. No published number for the internal CSC cost; nobody in the
  streaming space bothers pre-converting.
- **Register/unregister cost**: `nvEncRegisterResource` maps a D3D11 texture into the encoder's
  address space. It is documented as an expensive, once-per-resource operation; the SDK samples
  register the whole input pool at init and only `MapInputResource`/`UnmapInputResource` per
  frame. Map/unmap per frame is the SDK-sanctioned pattern (and is what we do); registering per
  frame is not.

### 2.4 Quick Sync / oneVPL

From the [Intel VPL encode structures reference](https://intel.github.io/libvpl/latest/API_ref/VPL_structs_encode.html):

- `LowDelayBRC`: "**When rate control method is `MFX_RATECONTROL_VBR`, `MFX_RATECONTROL_QVBR`
  or `MFX_RATECONTROL_VCM`** this parameter specifies frame size tolerance." It is **not
  defined for CBR**.
- `MaxFrameSizeI` / `MaxFrameSizeP`: "used in VBR based bitrate control modes"; recommended
  5×–10× the target frame size for I frames and 2×–4× for P/B. `MaxFrameSizeI` must be set if
  `MaxFrameSizeP` is; if `MaxFrameSizeP` is 0 the library copies `MaxFrameSizeI` into it.
  `AdaptiveMaxFrameSize` "is not supported with LowPower ON".
- `IntRefType`: `MFX_REFRESH_NO` / `VERTICAL` (MB columns) / `HORIZONTAL` (MB rows) / `SLICE`.
  `IntRefCycleSize` = pictures per refresh cycle, **minimum 2, values 0 and 1 invalid**,
  init-only. `IntRefCycleDist` = frames between the starts of cycles.
- `MaxSliceSize` = max slice size in bytes; "**when configured, overrides other slice-count
  controls**"; support must be checked with `MFXVideoENCODE_Query`. This is the MTU-sized-slice
  knob and is strictly better than a fixed `NumSlice` for a lossy link.
- `WinBRCSize` / `WinBRCMaxAvgKbps` — sliding-window cap; "applies to CBR, VBR, LA, LA_HRD and
  QVBR modes", so unlike `LowDelayBRC` these *are* live under CBR.
- `AsyncDepth = 1` is the documented way to say "no asynchronous pipelining"; it is the correct
  low-latency value.
- `LowPower = MFX_CODINGOPTION_ON` selects the VDEnc/fixed-function path, which is the low-latency
  encoder on Intel; `TargetUsage = MFX_TARGETUSAGE_BEST_SPEED` (7) is the fastest preset.
- `ScenarioInfo = MFX_SCENARIO_DISPLAY_REMOTING` is the correct hint for this workload.

**The cross-adapter problem.** D3D11 shared resources created with
`D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX` and opened with
`ID3D11Device1::OpenSharedResource1` are a **same-adapter** mechanism; sharing across two
physical adapters requires `D3D11_RESOURCE_MISC_SHARED_CROSS_ADAPTER`, which restricts the
resource to a cross-adapter heap that can only be a copy source/destination (no SRV, no RTV, no
video-processor view, no encoder input). OBS's own documentation of the problem is blunt:
"Texture encoding works by directly accessing the frame buffer of the app it captures, and this
is only possible if it runs on the same GPU"; OBS's QSV encoder "implements a soft fallback to
system memory mode if texture sharing isn't supported"
([OBS hardware encoders](https://deepwiki.com/obsproject/obs-studio/4.4.2-hardware-video-encoders)).
**Conclusion: the CPU bridge cannot be eliminated in the general dGPU-capture → iGPU-encode
case.** What *can* be eliminated is ever entering it — by making the capture device land on the
adapter that owns the encoder we intend to use (§2.6). A cross-adapter *copy* is still possible
(`SHARED_CROSS_ADAPTER` staging texture + `CopyResource` on both sides) and stays on the GPU
bus rather than going through a CPU map, but it is a full-frame PCIe round trip either way and
nobody in the streaming space ships it.

### 2.5 x264

`--preset ultrafast --tune zerolatency` sets `bframes=0`, `rc-lookahead=0`,
`sync-lookahead=0`, `sliced-threads=1`, `intra-refresh` off. From
[x264.h](https://code.videolan.org/videolan/x264/-/blob/master/x264.h):

- `void (*nalu_process)( x264_t *h, x264_nal_t *nal, void *opaque );` — "**Optional low-level
  callback for low-latency encoding. Called for each output NAL unit immediately after the NAL
  unit is finished encoding.**" This is x264's equivalent of NVENC's sub-frame write, and it is
  free: no polling, no extra thread.
- `int i_slice_max_size;` — "Max size per slice in bytes; includes estimated NAL overhead."
  `int i_slice_count;` — "Number of slices per frame: forces rectangular slices."
  `int i_slice_count_max;` — absolute cap.
- `int b_sliced_threads;` — slice-based threading; "adds no encoding latency" (frame-based
  threading buffers `threads` frames, slice-based does not).
- `#define X264_KEYINT_MAX_INFINITE (1<<30)`, `#define X264_CSP_BGRA 0x000f /* packed bgr 32bits */`,
  `#define X264_NAL_HRD_CBR 2`, `int b_repeat_headers; /* put SPS/PPS before each keyframe */`,
  `int b_annexb;`.

x264 achieves sub-10 ms end-to-end at low resolutions with zerolatency. The `X264_CSP_BGRA`
input path exists but note the in-tree blocker `blocker-x264-bgra-input-emits-444-stream.md` —
libx264 built for 4:4:4 will emit a High 4:4:4 stream from a BGRA input, which most browser
decoders reject.

### 2.6 D3D11 / OS scheduling

- **GPU scheduling priority.** OBS calls `D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(),
  D3DKMT_SCHEDULINGPRIORITYCLASS_REALTIME)`, resolved with `GetProcAddress(gdi32,
  "D3DKMTSetProcessSchedulingPriorityClass")`, *and then* `dxgiDevice->SetGPUThreadPriority(...)`
  ([obs-studio ec769ef](https://github.com/obsproject/obs-studio/commit/ec769ef008b748f7dfba211daec9eb203ea4bea0)).
  The commit exists precisely because `SetGPUThreadPriority` alone was not enough to stop the
  capture being starved by a full-screen game. `D3DKMTSetProcessSchedulingPriorityClass` is
  documented from Vista onward; REALTIME is the top class.
- **Sync primitive.** `IDXGIKeyedMutex` is the portable choice and the only one that works for a
  D3D11 NT-handle shared texture. D3D11.4 fences (`ID3D11Device5::CreateFence`, `ID3D11Fence`)
  are lower overhead but Windows 10 1703+ and only worth it if the keyed mutex shows up in a
  profile. The real hazard with keyed mutexes is the timeout: an `INFINITE` (Sunshine) or long
  (ours: 1000 ms) `AcquireSync` on a real-time capture thread converts a GPU hiccup into a
  one-second stall.
- **Timer resolution.** `timeBeginPeriod(1)` no longer affects other processes from Windows 10
  2004 onward, and from Windows 11 "if a window-owning process becomes fully occluded,
  minimized, or otherwise invisible… Windows does not guarantee a higher resolution than the
  default system resolution" — which is exactly the ScreenExtend host's normal state.
  `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION` is the documented opt-out and we already
  set it. Per-wait, `CreateWaitableTimerExW(..., CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, ...)`
  (Windows 10 1803+) gives ~500 µs without touching the global rate
  ([Bruce Dawson, "Windows Timer Resolution: The Great Rule Change"](https://randomascii.wordpress.com/2020/10/04/windows-timer-resolution-the-great-rule-change/)).
- **Hybrid CPUs (Alder Lake and newer, and Core Ultra).** `GetSystemCpuSetInformation` returns
  an `EfficiencyClass` per logical processor; `SetThreadSelectedCpuSetMasks` restricts a thread
  to a set of them. Intel's own
  [12th-gen game-dev guide](https://www.intel.com/content/www/us/en/developer/articles/guide/12th-gen-intel-core-processor-gamedev-guide.html)
  is the primary source: group CPU sets by `EfficiencyClass`, keep latency-critical threads on
  the highest class (P-cores). This is a **soft** restriction (unlike `SetThreadAffinityMask`),
  so the scheduler can still migrate under pressure — which is why it is the right API here.
- `SetPriorityClass(REALTIME_PRIORITY_CLASS)` is a genuine footgun: it outranks most kernel
  worker threads and can deadlock the input stack. `HIGH_PRIORITY_CLASS` (what we use) plus
  MMCSS "Pro Audio" per-thread is the correct combination and matches what audio and capture
  software ships.
- **HAGS / MPO.** Hardware-Accelerated GPU Scheduling changes who owns the submission queue but
  has no documented API surface for a capture app, and MPO (multi-plane overlay) can cause DXGI
  duplication to miss overlay-composited content on some drivers — a *correctness* consideration
  for the DXGI path (and one of the reasons WGC exists), not a latency knob.

### 2.7 Adapter selection on hybrid-graphics laptops

- `IDXGIFactory6::EnumAdapterByGpuPreference(index, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, ...)`
  returns adapters reordered so index 0 is the highest-performing (dGPU/xGPU). **Windows 10
  1803+**, i.e. below our floor
  ([IDXGIFactory6](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_6/nn-dxgi1_6-idxgifactory6),
  [EnumAdapterByGpuPreference](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_6/nf-dxgi1_6-idxgifactory6-enumadapterbygpupreference)).
  This is the runtime, no-relaunch replacement for writing
  `HKCU\Software\Microsoft\DirectX\UserGpuPreferences`.
- `DXGI_ADAPTER_DESC1.VendorId` is the PCI vendor ID: **0x10DE NVIDIA, 0x8086 Intel, 0x1002
  AMD**, 0x1414 Microsoft (WARP / Basic Render). `DXGI_ADAPTER_FLAG_SOFTWARE` is set on WARP.
  Matching on these is deterministic; matching on `Description` substrings is not.
- **Which adapter owns an indirect-display (IDD) monitor?** IddCx drivers can call
  `IddCxAdapterSetRenderAdapter` to nominate a *preferred render adapter* by LUID, and "the OS
  will use the IDD's preferred render adapter where possible" but "in some cases… the OS may
  choose a different adapter"
  ([IddCxAdapterSetRenderAdapter](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/nf-iddcx-iddcxadaptersetrenderadapter)).
  The OS then hands the driver the actual render adapter in `EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN`.
  **There is no documented guarantee, so this must be discovered at runtime, not assumed** — and
  our own code already discovers it: `find_output` (dxgi/mod.rs:73-108) walks every adapter's
  outputs looking for the GDI device name and logs `"dxgi: output {} found on adapter '{}'"`.
  That log line is the answer on any given box.
- Consequence: the DXGI path's D3D11 device **must** be created on the adapter that enumerates
  the output (duplication is adapter-local). The WGC path's device may be on any adapter, so it
  is free to be created on the encoder's adapter — which is the whole game for avoiding a CPU
  bridge.

---

## 3. How others do it

**Sunshine (`src/platform/windows/display_wgc.cpp`, `display_vram.cpp`, `src/nvenc/nvenc_base.cpp`)**
- WGC: `CreateFreeThreaded(uwp_device, format, 2, item.Size())`; `FrameArrived` handler stores
  only the newest frame (`if (produced_frame) produced_frame.Close(); produced_frame = frame;`)
  and signals a condition variable that the consumer thread waits on with a timeout. Feature
  probing is per-property `ApiInformation::IsPropertyPresent` for both `IsBorderRequired` and
  `MinUpdateInterval`; when `MinUpdateInterval` is present it sets a flat `4ms` (250 Hz) with a
  comment that without it capture is "capped to 60fps". **Absent properties are skipped, never
  fatal.**
- DXGI: change detection is `LastPresentTime != 0 || LastMouseUpdateTime != 0 ||
  PointerShapeBufferSize > 0`; anything else returns `capture_e::timeout`. Resolution/format
  change → `capture_e::reinit`.
- Cross-device textures: `MiscFlags = SHARED_NTHANDLE | SHARED_KEYEDMUTEX`,
  `IDXGIResource1::CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ, ...)`,
  `OpenSharedResource1` on the encoder device, RAII `texture_lock_helper` doing
  `AcquireSync(0, INFINITE)` / `ReleaseSync(0)`. **Same structure as ours, with a single key
  instead of our two-key writer/encoder handshake and an infinite timeout instead of 1 s.**
- NVENC: `tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`, `rateControlMode = NV_ENC_PARAMS_RC_CBR`,
  `zeroReorderDelay = 1`, `enableLookahead = 0`, `vbvBufferSize = bitrate*1000/framerate` with an
  optional `vbv_percentage_increase`, `gopLength = NVENC_INFINITE_GOPLENGTH`,
  `idrPeriod = NVENC_INFINITE_GOPLENGTH`, `frameIntervalP = 1`, `enablePTD = 1`,
  `repeatSPSPPS = 1`, `enableEncodeAsync = async_event_handle ? 1 : 0`.
  **`sliceMode = 3; sliceModeData = client_config.slicesPerFrame;`** — slices per frame are a
  *client-negotiated* parameter. Intra refresh is gated on both a client request and a cap
  query, with `intraRefreshPeriod = 300; intraRefreshCnt = 299;`. Reference frames default to
  **5** for H.264/HEVC (not 1). CABAC/CAVLC and weighted prediction are both cap-gated. **No LTR.**

**OBS Studio (`libobs-winrt/winrt-capture.cpp`, `libobs-d3d11/d3d11-subsystem.cpp`)**
- WGC: `Direct3D11CaptureFramePool::Create(device, format, 2, size)` (dispatcher-queue variant),
  single `TryGetNextFrame()` per callback, no drain loop. Support gate is
  `IsApiContractPresent(L"Windows.Foundation.UniversalApiContract", 8)`; cursor and border are
  each `IsPropertyPresent`-gated.
- Scheduling: `D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(),
  D3DKMT_SCHEDULINGPRIORITYCLASS_REALTIME)` via `GetProcAddress` on `gdi32`, then
  `SetGPUThreadPriority`.
- QSV: same-adapter texture encoding with a documented soft fallback to system memory when the
  encoder is not on the capture adapter.

**FFmpeg (`libavcodec/nvenc.c`)** — for `-tune ull` sets P1 + `ULTRA_LOW_LATENCY`, CBR,
`zeroReorderDelay`, one-frame VBV; exposes `-intra-refresh`, `-ldkfs` (`lowDelayKeyFrameScale`)
and `-no-scenecut`. It does not implement sub-frame write or LTR either.

**Nobody open-source implements NVENC sub-frame write, `sliceOffsets` readback, or LTR-based
loss recovery for desktop streaming.** Moonlight/Sunshine get most of the same benefit from
multi-slice encoding plus RTP-level FEC. That is worth knowing before spending on it.

---

## 4. What ScreenExtend does today

**Capture selection** — `pipeline.rs:177-211` / `252-289`: try WGC, and on *any* error fall back
to DXGI duplication. WGC settings are built at `pipeline.rs:314-332`:

```rust
CursorCaptureSettings::WithCursor,        // needs 19041
DrawBorderSettings::WithoutBorder,        // needs 20348 + Borderless consent
SecondaryWindowSettings::Default,
MinimumUpdateIntervalSettings::Custom(frame_duration * 2 / 3),  // needs 26100
DirtyRegionSettings::Default,             // never requested
ColorFormat::Bgra8,
```

`GraphicsCaptureApi::new` (graphics_capture_api.rs:148-176) converts *any* non-`Default`
setting whose property is absent into a hard `Err`, so on every host below build 26100 WGC
fails at construction and the DXGI path is what actually runs. That is
`blocker-wgc-min-update-interval-forces-dxgi.md`, and my version research narrows it: the
offending gate is **26100**, so this is not "Windows 10 only" — it is *every Windows 11 before
24H2 as well*.

**Frame pool** — `Direct3D11CaptureFramePool::Create(..., 3, item.Size()?)`
(graphics_capture_api.rs:197); `Recreate(..., 1, frame_content_size)` on a size change
(graphics_capture_api.rs:286-291). `FrameArrived` calls `TryGetNextFrame()` once
(graphics_capture_api.rs:264) with no drain. `start_free_threaded` (capture.rs:391) is a
misnomer: it spawns a thread that creates its **own** `DispatcherQueueController` and pumps
`GetMessageW` (capture.rs:39, 409) — `CreateFreeThreaded` is never called anywhere in the
fork.

**DXGI path** — `Duplicator::poll` (dxgi/mod.rs:397-482) does `AcquireNextFrame(timeout_ms)`
with `timeout_ms = 8` when idle and time-to-`next_due` when a frame is pending
(pipeline.rs:559-564); checks `LastPresentTime != 0`; calls `present_region_is_empty`
(dxgi/mod.rs:349-395) which reads `GetFrameMoveRects` + `GetFrameDirtyRects` and reduces both
to one boolean; `CopyResource` into `self.desktop`; then `ReleaseFrame()`. `AccumulatedFrames`
is read only inside `present_region_is_empty` (`> 1` → "not empty"); `RectsCoalesced` and
`ProtectedContentMaskedOut` are never read.

**Encode selection** — `build_backend` (pipeline.rs:1332-1456): Auto → NVENC → Intel → x264.
The WGC handler additionally short-circuits to a K=3 zero-copy NVENC ring
(pipeline.rs:1553-1610) before `build_backend` is consulted. The NVENC device is chosen by an
uppercased `"NVIDIA"` substring on `DXGI_ADAPTER_DESC1.Description`
(nvidia/encoder.rs:871-875); the WGC capture device is created with a **null adapter**
(windows_capture/d3d11.rs:203-213); the DXGI device is created on whichever adapter enumerates
the output (dxgi/mod.rs:111-146). Three different policies, none of them coordinated.

**NVENC config** (nvidia/encoder.rs:540-657) — P1 + `ULTRA_LOW_LATENCY`, infinite GOP + infinite
IDR period, `frameIntervalP = 1`, `enableLookahead(0)` + `lookaheadDepth = 0`, `enableAQ(0)`,
`enableTemporalAQ(0)`, `multiPass = DISABLED`, `zeroReorderDelay(1)`, `maxNumRefFrames = 1`,
`numRefL0 = NUM_REF_FRAMES_1`, `repeatSPSPPS(1)`, no filler, no AUD, no picture-timing SEI, BT.709
full-range VUI, `enableEncodeAsync = 0`, `sliceMode = 0 / sliceModeData = 0` (single slice),
`NV_ENC_BUFFER_FORMAT_ARGB` (nvidia/encoder.rs:687, 761). **The preset choice is correct and
should be preserved.** What is missing: any `nvEncGetEncodeCaps` call at all, `enableSubFrameWrite`,
`enableEncodeAsync`, `lowDelayKeyFrameScale`, `splitEncodeMode`, LTR, and reference-frame
invalidation.

**Intel config** (intel/encoder.rs:395-482) — `LowPower = ON`, `TargetUsage = BEST_SPEED`,
`GopPicSize = 0xFFFF`, `GopRefDist = 1`, `IdrInterval = 0`, `NumRefFrame = 1`, `AsyncDepth = 1`,
`IOPattern = IN_VIDEO_MEMORY | OUT_VIDEO_MEMORY`, `ScenarioInfo = MFX_SCENARIO_DISPLAY_REMOTING`,
`NalHrdConformance = OFF`, `RateControlMethod = CBR`,
`NumSlice = (height / 256).clamp(4, 8)`, and — under `if self.config.qp.is_none()`, i.e. exactly
when CBR is selected — `LowDelayBRC = ON`, `WinBRCSize = 1`, `WinBRCMaxAvgKbps = max_kbps`,
`MaxFrameSizeP = 2× avg`, `MaxFrameSizeI = 6× avg`. **Per the oneVPL reference, `LowDelayBRC`
is only defined for VBR/QVBR/VCM and `MaxFrameSize*` for "VBR based" modes, so those three
lines are almost certainly no-ops under CBR.** `MaxSliceSize` is never used.

**x264 config** (x264/encoder.rs:47-135) — `param_default_preset("ultrafast", "zerolatency")`,
then `i_csp = X264_CSP_I420`, `b_sliced_threads = 1`, `i_threads = encode_thread_count()`,
`rc.i_lookahead = 0`, `i_sync_lookahead = 0`, `rc.i_rc_method = X264_RC_ABR`,
`i_vbv_max_bitrate = kbps`, `i_vbv_buffer_size = vbv_buffer_kbit(kbps, fps)`,
`i_keyint_max = X264_KEYINT_MAX_INFINITE`, `b_repeat_headers = 1`, `b_annexb = 1`, then a BGRA
variant tried first. `nalu_process` exists in the binding (`x264_sys.rs:295`) and is never set;
`i_slice_max_size` / `i_slice_count` (`x264_sys.rs:286-290`) are never set; `i_nal_hrd`
(`x264_sys.rs:207`) is never set.

**Tuning** (tuning.rs, all of it) — process: registry `UserGpuPreferences=GpuPreference=2`,
`timeBeginPeriod(1)`, `PROCESS_POWER_THROTTLING_EXECUTION_SPEED` opt-out,
`PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION` opt-in, `SetPriorityClass(HIGH)`. Per
capture/encode thread: MMCSS ("Pro Audio" → "Games" → "Playback") at `AVRT_PRIORITY_CRITICAL`,
`SetThreadPriority(TIME_CRITICAL)`, `SetThreadPriorityBoost(true)`, plus
`IDXGIDevice::SetGPUThreadPriority(5)`. Transport threads get MMCSS "Playback" +
`ABOVE_NORMAL`. `SetThreadExecutionState` keep-awake. **Missing:
`D3DKMTSetProcessSchedulingPriorityClass`, any P-core/E-core awareness, and any high-resolution
waitable timer.**

---

## 5. Recommendations

Ranked by expected latency delta ÷ implementation cost. Each names the API, the version floor,
the runtime probe, the expected win, and the measurement. "**[new]**" means a `findings/` file
was written for it by this report; the others already have one, cited by name.

### R1 — Make every WGC feature request independently optional (blocker, ~2 h)
`MinUpdateInterval` and `DirtyRegionMode` are **26100+**, `IsBorderRequired` is 20348+ *and*
needs `GraphicsCaptureAccess.RequestAccessAsync(Borderless)`, `IsCursorCaptureEnabled` is
19041+. Change `GraphicsCaptureApi::new` so an absent property logs and skips the setter
instead of returning `Err`, and change `Settings` from "Default | Custom" to "Default |
Preferred(x) | Required(x)" so callers can express "nice to have". Probe with
`ApiInformation::IsPropertyPresent` exactly as today — the probe is already correct, only the
*response* is wrong. **Win: WGC becomes reachable at all on ~every host that isn't Win11 24H2+**;
whether that is faster than DXGI is R2's job to establish. Covered by
`blocker-wgc-min-update-interval-forces-dxgi.md`.

### R2 — Measure WGC vs DXGI on the dev box before optimising either (free, and gates R3/R4)
Both paths exist. After R1, force each with a config flag and compare glass-to-glass. Nobody
has published a credible number and the community consensus is contradictory. **Do this first.**

### R3 — Drain the WGC frame pool to the newest frame, and use 2 buffers (major, ~1 h)
`TryGetNextFrame` dequeues the oldest; with 3 buffers a consumer that falls one frame behind
serves a 2-frame-old picture forever. Loop `TryGetNextFrame` until it returns null, keep the
last, close the rest — this is Sunshine's behaviour expressed explicitly. Drop the pool to 2
(both OBS and Sunshine). Also fix `Recreate(..., 1, ...)` → the same count.
**Win: up to 2 frame periods (33 ms at 60 fps) of tail latency.**
Covered by `major-wgc-framepool-never-drains-to-the-newest-frame.md` and
`major-wgc-framepool-recreate-drops-to-one-buffer.md`.

### R4 — Skip unchanged frames on the WGC path (major, ~3 h)
The DXGI path already does this (`present_region_is_empty`). WGC has two mechanisms:
`DirtyRegionMode::ReportOnly` + `Frame::dirty_regions()` on 26100+, and — available on *every*
build — comparing `Direct3D11CaptureFrame::SystemRelativeTime()` against the previous frame's,
since WGC re-delivers the same content on a heartbeat. Probe `DirtyRegionMode` with
`IsPropertyPresent` and fall back to the timestamp comparison. **Win: on an idle desktop the
encode and the 4-deep broadcast queue both go quiet; the keepalive path already exists.**
Covered by `major-wgc-path-has-no-unchanged-frame-skip.md` and
`speculative-wgc-dirty-regions-captured-but-never-used.md`.

### R5 — One adapter-selection policy for the whole Windows backend (major, ~1 day) **[new]**
Replace three uncoordinated policies with a single `select_adapters()` that:
1. finds the output for the target monitor and records its adapter LUID (`find_output` already
   does this) — the DXGI duplication device is pinned there, no choice;
2. picks the encoder adapter with `IDXGIFactory6::EnumAdapterByGpuPreference(0,
   DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)` (Win10 1803+, `QueryInterface` to probe), then
   classifies by `DXGI_ADAPTER_DESC1.VendorId` (0x10DE / 0x8086 / 0x1002) and
   `DXGI_ADAPTER_FLAG_SOFTWARE`, never by description substring;
3. creates the **WGC** device on the chosen encoder adapter (WGC does not require the display's
   adapter), so the shared-texture zero-copy path is same-adapter by construction;
4. logs both LUIDs and the resulting path name once.
Delete the `HKCU\...\UserGpuPreferences` write, which only takes effect on the *next* launch.
**Win: turns a silent full-frame GPU→CPU→GPU round trip (the CPU bridge) into a zero-copy path
on hybrid laptops, plus removes the first-run-after-install regression.** Supersedes
`speculative-wgc-device-lands-on-the-wrong-adapter.md`,
`minor-nvenc-adapter-chosen-by-name-substring.md` and
`minor-tuning-gpu-preference-applies-next-launch.md` as symptoms.

### R6 — NVENC async mode with a bitstream buffer and completion event per ring slot (major, ~1 day)
Already filed as `major-nvenc-sync-single-bitstream-defeats-ring.md`; the SDK guide confirms it
is the documented design ("`NvEncEncodePicture`… is non-blocking", "asynchronous mode… supported
only on Windows 10 and above"). Probe `NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT` (30) and keep the
synchronous path when it is 0. **Win: the GPU encode duration leaves the critical path (single
digits of ms at 1080p, more at 1440p/4K).**

### R7 — NVENC multi-slice + `enableSubFrameWrite` (major, ~2 days, gated on R6)
Set `sliceMode = 3` / `sliceModeData = N` (Sunshine's shape: slices as a negotiated parameter)
and `enableSubFrameWrite = 1`, then poll `nvEncLockBitstream`. Probe
`NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK` (23) and `SUPPORT_DYNAMIC_SLICE_MODE` (27). Note the
exclusivity: `reportSliceOffsets` requires `enableEncodeAsync = 0`, so with R6 in place the
sub-frame *write* is the route, not the offsets array. This also unblocks intra refresh, which
on many parts needs multiple slices unless `NV_ENC_CAPS_SINGLE_SLICE_INTRA_REFRESH` (50) is 1.
**Win: up to (encode_time × (N-1)/N) removed, plus each slice becomes an independently
loss-recoverable unit.** Covered by `major-nvenc-single-slice-no-subframe-readback.md`.

### R8 — Call `nvEncGetEncodeCaps` before asserting anything (major, ~half a day)
Zero calls exist. Every setting in R6/R7 plus CABAC, constrained encoding, intra refresh, LTR
and rate-control mode should be cap-gated, and the results logged once. **Win: no latency, but
it is the precondition for every other NVENC change and for the tier doctrine.** Covered by
`major-nvenc-no-capability-probing.md`.

### R9 — Fix the Intel rate control: CBR ignores `LowDelayBRC` and `MaxFrameSize*` (major, ~half a day) **[new]**
`MFX_RATECONTROL_VCM` (video-conferencing mode) is the mode oneVPL documents these knobs for and
is designed for exactly this workload. Probe by filling the params and calling
`MFXVideoENCODE_Query` — if VCM is rejected, fall back to CBR and *drop* the three dead lines so
the log stops claiming settings that do not apply. Keep `WinBRCSize`/`WinBRCMaxAvgKbps`, which
the docs say do apply under CBR. **Win: a real per-frame size cap, so an IDR no longer produces
a multi-frame-period transmit burst.**

### R10 — `D3DKMTSetProcessSchedulingPriorityClass(REALTIME)` (major, ~1 h) **[new]**
`GetProcAddress(gdi32, "D3DKMTSetProcessSchedulingPriorityClass")` — probe by the presence of
the export, ignore failure. OBS added this specifically because `SetGPUThreadPriority` alone did
not stop capture starvation under full-screen GPU load. **Win: removes multi-frame stalls when
the host is also gaming; no effect on an idle host.**

### R11 — Intel `MaxSliceSize` instead of a fixed `NumSlice` (minor, ~2 h) **[new]**
`NumSlice = (height/256).clamp(4, 8)` is a guess. `MaxSliceSize` "overrides other slice-count
controls" and gives MTU-shaped slices. Probe with `MFXVideoENCODE_Query`; keep `NumSlice` when
unsupported. **Win: fewer fragmented RTP packets, better loss granularity; small latency effect.**

### R12 — x264 slice-level output via `nalu_process` (major on the software path, ~1 day) **[new]**
`nalu_process` is x264's zero-cost sub-frame write and the binding already has the field. Set it
plus `i_slice_max_size` (or `i_slice_count`) and push each NAL to the RTP packetizer as it
appears, instead of after `x264_encoder_encode` returns. **Win: on the CPU fallback, where a
1080p frame can take 8–15 ms to encode, this removes most of it from the critical path.** Costs:
the callback runs on an x264 worker thread, so the packetizer hand-off must be lock-free.

### R13 — Pin capture/encode threads to P-cores on hybrid CPUs (major, ~half a day) **[new]**
`GetSystemCpuSetInformation` → group by `EfficiencyClass` → `SetThreadSelectedCpuSetMasks` with
only the highest class. Probe by the presence of more than one `EfficiencyClass`; do nothing on
a homogeneous CPU. Soft affinity, so no starvation risk. **Win: removes the occasional 10–30 ms
outlier when MMCSS parks a TIME_CRITICAL thread on an E-core. Nothing on the dev box if its CPU
is homogeneous — verify with `GetLogicalProcessorInformationEx`.**

### R14 — x264 CBR + HRD instead of ABR (minor, ~1 h) **[new]**
`rc.i_rc_method = X264_RC_ABR` with a VBV is not CBR; `i_nal_hrd = X264_NAL_HRD_CBR` makes the
stream HRD-conformant and the VBV actually binding, which is what a fixed-rate link wants.
**Win: bounded per-frame size on the software path.**

### R15 — `MinUpdateInterval` as a flat floor, not a ratio (minor, ~15 min) **[new]**
`frame_duration * 2 / 3` gives 11.1 ms at 60 fps and 5.5 ms at 120 fps. The measured data in
`windows-capture#190` says the delivered rate is well below `1/interval`, so the interval should
be a small constant (Sunshine uses 4 ms / 250 Hz) rather than a fraction of the target period.
**Win: reaches the target frame rate on high-refresh virtual displays.**

### R16 — `splitEncodeMode` (speculative, ~2 h) **[new]**
Probe `NV_ENC_CAPS_NUM_ENCODER_ENGINES` (49); when > 1, leave `NV_ENC_SPLIT_AUTO_MODE` (which
P1–P4 already select implicitly) and log the engine count. **Zero win on the single-NVENC
Ampere laptop dev box; up to ~1 frame at 8K and large throughput headroom on Ada/Blackwell.**

### Explicitly *not* recommended
- `enableOutputInVidmem` — we consume the bitstream on the CPU; it would add a readback.
- Pre-converting BGRA→NV12 with the D3D11 VideoProcessor before NVENC — NVENC's internal ARGB
  path is cheaper, and `NV_ENC_BUFFER_FORMAT_ARGB` already matches a DXGI BGRA texture byte for
  byte.
- `DuplicateOutput1` — no latency benefit for SDR BGRA8.
- `SetPriorityClass(REALTIME_PRIORITY_CLASS)` — can starve kernel worker threads; `HIGH` plus
  MMCSS is the shipping-software norm.
- Cross-adapter `SHARED_CROSS_ADAPTER` textures to keep Quick Sync on an iGPU while capturing a
  dGPU display — the resource can only be a copy source/destination, so it is a full-frame PCIe
  round trip either way. Fix the adapter choice (R5) instead.
- Per-region / dirty-rect encoding via `qpDeltaMap` — changes quality, not encode cost.

---

## 6. Verification plan

**Instrument first.** The existing `encode-path latency: path=…, avg_ms, max_ms` line
(pipeline.rs:636-649, 1748-1757) measures capture-callback → AU-ready. Split it into stages so
each recommendation has a number to move:
`t_frame_stamp` (`Direct3D11CaptureFrame::SystemRelativeTime` / `DXGI_OUTDUPL_FRAME_INFO::LastPresentTime`,
both QPC) → `t_callback` → `t_staged` (after `CopyResource`/scale + `Flush`) → `t_submit`
(after `nvEncEncodePicture`) → `t_bitstream` (after `nvEncLockBitstream`) → `t_broadcast`.
Report p50/p95/p99, not just avg/max.

Also log, once per session: chosen capture path, capture adapter LUID + `VendorId`, encoder
adapter LUID + `VendorId`, whether the two match, the WGC properties present/absent, and every
`nvEncGetEncodeCaps` result. Half the recommendations here are unverifiable without that line.

**Measurable on the Windows dev box (Win11 26200 + RTX 3080 Ti Laptop + Intel iGPU + the IDD):**
- R1–R4: the WGC path can finally run, so WGC vs DXGI is directly comparable at 1080p60,
  1440p120 and 4K60 on the virtual display.
- R5: the adapter log line answers "which adapter owns the IDD output" empirically, and the path
  name flips from `cpu-bridge` to `zero-copy-ring` (or does not, which is itself the answer).
- R6, R7, R8, R16: all NVENC-side; caps are queryable and 26200 satisfies async mode's Win10+
  requirement. R16 will report 1 engine and do nothing — that is a valid negative result.
- R10, R14, R15: directly observable.
- R12: x264 is forced with `--software-encode` / `disableGpuEncode`.
- Glass-to-glass end-to-end: point a 240 fps phone camera at the host screen and the client
  screen showing the same high-contrast flash, or use the client's existing
  `transform-worker.js` display-lag report (which already inverts the RTP timestamp back to host
  capture time) as the cheap continuous metric.

**Not verifiable on the dev boxes:**
- R9 and R11 (Intel): the iGPU is present, but whether the *virtual display's* frames ever land
  on it depends on R5's outcome; VCM support varies by generation and needs a second Intel-only
  host to be sure.
- R13: only meaningful if the dev box's CPU is hybrid. Check `EfficiencyClass` count first; if
  it is 1, this needs an Alder-Lake-or-newer host.
- R1's downgrade behaviour on **Windows 10 19041/19045 and Windows 11 22H2/23H2** — those are
  the builds where WGC currently fails, and there is no such host available. This needs a VM or
  a second machine; a VM is adequate because the question is "does the session construct", not
  "how fast".
- R16 on a multi-NVENC GPU (Ada/Blackwell desktop).
- NVENC's 8-session ceiling with 9+ simultaneous clients.
- The macOS box is irrelevant to all of this.

---

## 7. Open questions

1. **Is WGC actually faster than DXGI duplication here?** No credible public measurement exists,
   and the answer may be different for an IDD virtual display than for a physical panel. R2.
2. **Which adapter enumerates the ScreenExtend virtual display's output on a hybrid laptop?**
   `IddCxAdapterSetRenderAdapter` makes it a driver preference the OS may override, so it is
   per-machine. Our own log line answers it; nothing in the docs guarantees it.
3. **Does the WGC capture device have to be on the display's adapter at all?** The docs do not
   say it does, and creating it on the encoder's adapter is the entire basis of R5. If WGC
   silently inserts its own cross-adapter copy, R5's win evaporates — measure `t_callback →
   t_staged` before and after.
4. **Is `enableSubFrameWrite` worth its polling loop for a ~10 ms encode?** Nobody
   open-source ships it. R7 should be prototyped and measured before it is committed.
5. **Does `MFX_RATECONTROL_VCM` survive `LowPower = ON` on the target iGPUs?** The docs already
   flag one interaction (`AdaptiveMaxFrameSize` is unsupported with `LowPower ON`); there may be
   more.
6. **Is a K=3 ring plus a 2-buffer WGC pool plus a 4-slot broadcast channel too much total
   buffering?** Each is individually defensible; nobody has counted the worst-case total frames
   in flight. Worth a single audit once R3 and R6 land.
7. **The 1000 ms `KEY_TIMEOUT_MS` on the keyed mutex** (nvidia/encoder.rs:29) converts a GPU
   hiccup into a one-second stall on a TIME_CRITICAL thread. Sunshine uses `INFINITE`, which is
   worse. What is the right value — and should a timeout instead drop the frame? Filed as
   `minor-keyed-mutex-one-second-timeout-stalls-the-capture-thread.md`; the research did not
   settle it.
