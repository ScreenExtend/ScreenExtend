# Manual test: the AMD AMF H.264 backend

**Why this exists:** no machine on this project has an AMD GPU, so
`windows_utils/streamer/amd/` is `UNVERIFIED` — its FFI is fully asserted by `cargo test`, but
nothing in it has ever encoded a frame. This script is everything that needs a Radeon, in the
order that makes it one sitting.

**What you need**

- A Windows box with an AMD GPU: a Radeon desktop, or a Ryzen laptop with an AMD iGPU. The
  encoder needs VCN/VCE H.264, which is every AMD part since Raven Ridge/Vega **except** Navi 24
  (RX 6400 / RX 6500), where AMD removed the AVC encoder. If you are on an RX 6400/6500, step 2
  will fail and that is correct behaviour, not a bug.
- The Radeon/Adrenalin driver installed (that is what ships `amfrt64.dll`).
- `ffprobe` and `ffplay` on `PATH`.
- A phone or tablet on the same network for the end-to-end steps.
- Roughly 45 minutes.

**Record as you go.** Every step says what to paste back. Add a row to
`docs/OPTIMIZATIONS.md` and flip the `docs/CAPABILITY-TIERS.md` status from `UNVERIFIED` to
`VERIFIED` (or file what broke) at the end.

---

## 0. Machine facts (2 min)

```powershell
# GPU, driver
Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion, AdapterCompatibility
# the AMF runtime
Get-Item C:\Windows\System32\amfrt64.dll | Select-Object Length, LastWriteTime
```

Paste both. The Adrenalin version matters: `USAGE = ULTRA_LOW_LATENCY` is known to break
`Init()` on pre-RDNA parts from **23.7.1** onward, which is exactly what step 3 is checking.

---

## 1. Nothing linked against AMF (1 min)

```powershell
dumpbin /imports .\src-tauri\target\release\ScreenExtend.exe | Select-String -Pattern amf
```

**Must print nothing.** If `amfrt64` shows up in the import table, something linked instead of
`libloading`-ing it, and the binary will refuse to start on every machine *without* a Radeon
driver. This is the Windows analogue of the macOS `nm -u` rule in `CLAUDE.md`.

Then, on the same box, the tests that need no hardware:

```powershell
cd src-tauri
cargo test --lib windows_utils::streamer::test::amd_layout
```

13 tests, all passing. On this machine `amf_loader_handles_a_missing_runtime` takes its *other*
branch — the DLL is present, so it asserts `AMFQueryVersion` returns non-zero. That is the first
real signal that the FFI works.

---

## 2. Does AMF exist, and is it hardware? (5 min)

```powershell
cd src-tauri
cargo run --release -- --probe-encode --encoder amd out_amd.h264
```

Expected log lines, in order:

```
AMF runtime version 1.5.x.x
DXGI adapter (index=0, name=..., vendor_id=0x1002, is_software=false)
selected AMD adapter for AMF (index=0, name=...)
AMF AVC caps: accel=hardware(VCN/VCE), max_bitrate=..., streams=..., hw_instances=...,
              max_profile=..., max_level=..., max_refs=..., b_frames=..., pre_analysis=...,
              query_timeout=..., slice_output=..., color_conversion=..., throughput=.../... MB,
              input=...x......x..., bgra_native=Some(...), nv12_native=Some(...)
AMD AMF H.264 encoder initialized (D3D11 / ULTRA_LOW_LATENCY): 1920x1080@60, ...
```

**Paste the whole `AMF AVC caps:` line.** It answers, on hardware, every open question the
research could not: whether `QueryTimeout` is honoured, whether slice output exists, whether
BGRA and NV12 are `native` inputs, and how much VCN throughput other apps have already taken.

Failure modes and what they mean:

| Log | Meaning |
|---|---|
| `loading amfrt64.dll ...` error | No AMF on this box. Reinstall the Radeon driver; error 126 also means a missing VC runtime. |
| `no AMD adapter found` | The vendor-id match failed — paste the `DXGI adapter` lines. |
| `this adapter has no AMF H.264 encoder` | `CreateComponent` refused. Expected on Navi 24 (RX 6400/6500). |
| `AMF reports acceleration type software(cpu)` | Refused on purpose: x264 is the better software path. |

---

## 3. Which usage tier did it get? (2 min)

This is the tier question. Look for **exactly one** of these three shapes in the step-2 log:

| Tier | Log evidence | Meaning |
|---|---|---|
| **AMF ULL** | `AMD AMF H.264 encoder initialized (D3D11 / ULTRA_LOW_LATENCY)` and **no** `falling back` line | The good path. |
| **AMF LL** | `AMF: ULTRA_LOW_LATENCY usage rejected by this driver/GPU; falling back to LOW_LATENCY` **or** `AMF Init() failed under ULTRA_LOW_LATENCY (...); retrying with LOW_LATENCY`, then `initialized (D3D11 / LOW_LATENCY)` | The AMF#410 workaround fired. Expected on pre-RDNA (Polaris/Vega/RDNA1) with driver 23.7.1 or newer. Still a hardware encode; note *which* of the two messages appeared. |
| **x264** | `AMD AMF unavailable (...); falling back to software x264` then `SOFTWARE x264 CPU path` | No AMF encode at all. Paste the error. |

Also look for a read-back mismatch line:

```
AMF: Usage read back as ... after setting ...
```

and, if you are on GCN (RX 580, Vega), check what `rc=` says in the `initialized` line. AMD are
reported not to allow `RateControlMethod` to be overridden after ULL on those parts, so `rc=`
may say `LatencyVBR` rather than `CBR`. **That is an acceptable outcome** with a one-frame VBV —
record it, do not "fix" it.

**Forcing each tier by hand:**

- ULL vs LL: there is no flag. To exercise the fallback deliberately, temporarily change
  `probe_ultra_low_latency` in `amd/encoder.rs` to return `false` and re-run — you should get the
  `rejected by this driver/GPU` line and a working `LOW_LATENCY` encode.
- AMF vs x264: `--encoder amd` forces AMF (falling back to x264 only on failure);
  `--encoder software` forces x264; `--encoder auto` is the shipping chain
  (NVENC then Intel then AMF then x264).

---

## 4. Is the bitstream what we asked for? (5 min)

```powershell
ffprobe -v error -select_streams v:0 -show_entries stream=codec_name,profile,level,has_b_frames,color_range,color_space -of default=nw=1 out_amd.h264
ffprobe -v error -select_streams v:0 -show_entries frame=pict_type -of csv=p=0 out_amd.h264 | Sort-Object -Unique
ffplay out_amd.h264
```

Assert:

- `codec_name=h264`, `has_b_frames=0`.
- `pict_type` values are only `I` and `P` — **never `B`**.
- The first frame is `I` and there is exactly **one** of them in a 300-frame probe (infinite GOP:
  keyframes are on demand only). More than one means `IDRPeriod` did not take.
- `color_range` follows the **input format**, which the `AMF input is ...` / `AMF re-initializing
  for a ...` log lines name. Packed BGRA off the capture is full range, so `pc`; NV12 out of the
  scaler comes from the D3D11 video processor in BT.709 **limited**, so `tv`. Signalling full
  range over limited-range samples is a washed-out picture, so a mismatch between the log line
  and this field is a real bug, not a cosmetic one.
- `color_space`, `color_primaries` and `color_transfer` are very likely **absent**, and that is
  the known AMF driver bug (GPUOpen AMF issues #398, #277, #540: `OutColorProfile`,
  `OutColorPrimaries`, `OutColorTransferChar` and `OutMatrixCoeff` never reach the bitstream on
  any codec; `FullRangeColor` is the only one that works). The encoder sets all five anyway, so
  if a future driver starts honouring them these fields appear on their own. Record what this
  driver does either way — that is the whole point of asking.
- Then compare against an NVENC capture of the same content on an NVIDIA box:
  `ffprobe -show_entries stream=color_range,color_space,color_primaries,color_transfer` on both,
  and render both in the client page on the *same* device. If they look different, the fix is
  ours and cheap (we hand-packetize H.264, so the SPS VUI can be patched on the way out); if they
  look the same, the browser is inferring BT.709 for HD and nothing needs doing. **Record which.**
- `ffplay` shows the moving cyan box on a scrolling gradient, not garbage.

Then confirm the first access unit carries SPS and PPS in band:

```powershell
ffprobe -v error -show_packets -of csv out_amd.h264 | Select-Object -First 3
```

---

## 5. Encode latency (10 min)

Live capture, so the per-60-frame timing log runs:

```powershell
cargo run --release -- serve -v --encoder amd
```

Join from the phone, leave it up for **five minutes**, and watch:

```
encode-path latency: path=amd-amf-same-adapter, avg_ms=..., max_ms=...
```

Targets, from the NVENC band on comparable hardware:

| Resolution | avg | max |
|---|---|---|
| 1080p60 | single-digit ms | under about 2x avg |
| 1440p60 | low teens | under about 2x avg |
| 4K60 | teens | under about 2x avg |

**Watch the max, not the average.** A creeping max over five minutes is a VBV or buffering
regression and is where latency bugs hide. Paste the first and last timing lines of the run.

Also confirm `path=amd-amf-same-adapter`. If it says `amd-amf-cpu-bridge`, the capture device
was not the AMD adapter — paste the `capture device adapter:` line, because on an all-AMD box
that should not happen.

And confirm this line **never** appears:

```
AMF SubmitInput returned AMF_INPUT_FULL under one-frame-in-flight discipline
```

If it does, one-in-flight discipline is broken somewhere upstream.

---

## 6. Glass-to-glass (10 min)

The project's photographic method: a millisecond stopwatch running on the extended display,
photographed side by side with the receiving browser. Ten photos, report the median and the
worst. Do the same run with `--encoder software` for a same-machine baseline.

Confirm the client is hardware-decoding: on the phone/tablet, `chrome://media-internals`, the
active player, `video_decoder` should name a hardware decoder rather than `FFmpegVideoDecoder`.

---

## 7. Adaptive bitrate and on-demand IDR (5 min)

```powershell
cargo run --release -- --probe-bitrate --encoder amd
```

The byte rate must follow the stepped target within a few frames, with **no resolution change**
and **no IDR storm**. In the live session, the log should show:

```
adapting bitrate: ... -> ... bps
AMF bitrate reconfigured (bitrate_bps=...)
```

Then force a keyframe (reload the client page, or let the browser send a PLI by briefly dropping
its network) and confirm **exactly one** IDR results — not a burst.

---

## 8. Leaks and restarts (5 min)

Join and leave from the client 20 times in a row while `serve -v` runs. Watch in Task Manager
(Details, then add the *Handles* and *GPU memory* columns): both must return to their baseline.
This is really a test of the `Drain` then `Terminate` then `Release` ordering, and of the
`libloading::Library` being the last dropped field.

Then check the log has **no** stray AMF text outside `logbus` — AMF's console trace writer is
supposed to be silenced at init. If AMF messages appear interleaved with ours, say so.

---

## 9. Cross-adapter (optional, 5 min — needs an Intel-iGPU + Radeon-dGPU laptop)

On a hybrid laptop the capture device is the iGPU, so `Backend::Amf` will refuse
("AMD AMF requires an AMD D3D11 device") and `try_amf` falls to `Backend::AmfCpu`. Confirm the
log says `AMD AMF CPU-bridge path`, that 150 frames arrive clean, and note the encode-latency
delta against the same-adapter numbers. Note also whether `--encoder auto` picked Intel instead,
which on that machine is the correct choice.

---

## 10. What ULL's free intra refresh costs (10 min)

The AMF encode doc's per-USAGE defaults give `IntraRefreshMBsNumberPerSlot` = **255** for both
`ULTRA_LOW_LATENCY` and `LOW_LATENCY`, and 0 for everything else. At 1080p (8160 MBs) that is a
~32-frame wave, about 0.53 s at 60 fps. The backend does not set the property, so on AMD we ship
a permanent rolling refresh **on** by default while NVENC ships it **off** by default
(`Config::intra_refresh`) — an inconsistency nobody chose, and nobody has published what it
costs. A rolling refresh raises the per-frame bit cost against a fixed CBR budget, and ours is
fixed (`VBVBufferSize = bitrate/fps`, peak == target), so any cost is paid in picture quality.

Encode the same 300-frame synthetic clip twice at a fixed 10 Mbps, once with the ULL default and
once with the refresh explicitly off, and compare size and quality:

```powershell
# ULL's default: the backend leaves IntraRefreshMBsNumberPerSlot alone, so the usage's 255 stands.
ScreenExtend probe --probe-encode --encoder amd --intra-refresh on  out_amd_refresh.h264
# Explicitly off: the backend writes IntraRefreshMBsNumberPerSlot = 0, which is the only way to
# turn the usage's default off.
ScreenExtend probe --probe-encode --encoder amd --intra-refresh off out_amd_norefresh.h264

ffprobe -v error -show_entries format=size -of csv=p=0 out_amd_refresh.h264
ffprobe -v error -show_entries format=size -of csv=p=0 out_amd_norefresh.h264
```

At a fixed CBR target the two files should be nearly the same size, so the cost shows up as
quality, not bytes. Measure it:

```powershell
ffmpeg -i out_amd_norefresh.h264 -i out_amd_refresh.h264 -lavfi "[0][1]ssim;[0][1]psnr" -f null -
```

Report the SSIM/PSNR delta and the file sizes. Anything under ~2% SSIM is noise and the ULL
default should stay; a 10-15% quality cost is resilience the transport may already be providing
through NACK/RTX, and then the refresh should be tied to `Config::intra_refresh` the way NVENC's
is. Also watch the picture for the banding the NVENC CLI help warns about — a refresh wave can be
visible as bands repainting the image.

---

## 11. The NV12 input path (5 min)

W3a's scaler can hand the CPU bridge NV12, and `Backend::AmfCpu` now asks for it. That is the
whole point: AMD state that packed-RGB input silently invokes an internal GPU colour converter
which competes with the foreground application, and OBS refuses BGRA on its texture path for the
same reason. NV12 removes that converter.

The switch happens on the first frame, through `retune_input_format` — the same path a GPU-side
format change takes. Look for, in order:

```
AMF re-initializing for a DXGI_FORMAT_NV12 input (was DXGI_FORMAT_B8G8R8A8_UNORM)
```

and **no** subsequent `AMF input is BGRA rather than NV12` line. Then confirm:

- 300 frames arrive clean after the retune, and `ffplay` shows the pattern, not garbage or
  swapped chroma. Chroma read as luma looks like a green or magenta cast over the whole frame.
- The IDR count is 2, not 1: `retune_input_format` sets `idr_requested`, because the component
  was re-`Init`ed and the client needs a fresh sequence.
- `color_range=tv` per section 4.
- The encode-path latency after the retune, against the BGRA numbers from step 5. This is the
  number the internal converter was costing.

---

## What to report back

1. The `AMF AVC caps:` line, verbatim.
2. The tier from step 3 (ULL / LL / x264) and which message proved it.
3. `has_b_frames`, the `pict_type` set, the IDR count, and `color_range`.
4. First and last `encode-path latency:` lines of the five-minute run.
5. Glass-to-glass median and worst, AMF vs x264 on the same box.
6. Anything in the log you did not expect — especially `AMF optional property not applied:`
   lines, which tell us which knobs this driver and VCN generation actually have.
7. Step 10's SSIM/PSNR delta and file sizes, and whether the refresh wave is visible.
8. Step 11's retune line, the post-retune encode latency against step 5, and `color_range`.
9. Whether the four `Out*` colour properties reached the bitstream on this driver, and whether an
   AMD stream and an NVENC stream of the same content look different in the client page.
