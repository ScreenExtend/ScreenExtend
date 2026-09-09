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
cargo test --lib windows_utils::streamer::amd::sps
```

14 and 8 tests, all passing. On this machine `amf_loader_handles_a_missing_runtime` takes its
*other* branch — the DLL is present, so it asserts `AMFQueryVersion` returns non-zero. That is
the first real signal that the FFI works.

---

## 2. Does AMF exist, and is it hardware? (5 min)

The harness is reached as the `probe` subcommand, and the **output path goes immediately after
the mode flag** — a path that follows another flag is not picked up and the run silently writes
to `out.h264`.

```powershell
cd src-tauri
cargo run --release -- probe --encode out_amd.h264 --encoder amd
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
AMF optional properties: LowLatencyInternal=1, RateControlPreanalysisEnable=0, ...
AMF SPS/PPS ready (extradata_bytes=...); SPS VUI rewritten for BT.709 full range and ...
AMD AMF H.264 encoder initialized (D3D11 / ULTRA_LOW_LATENCY): 1920x1080@60, ...
```

**Paste the whole `AMF AVC caps:` line and the whole `AMF optional properties:` line.** Between
them they answer, on hardware, every open question the research could not: whether
`QueryTimeout` is honoured, whether slice output exists, whether BGRA and NV12 are `native`
inputs, how much VCN throughput other apps have already taken, and — property by property —
which optional knobs this driver and VCN generation actually have.

Reading the properties line: `name=v` was written and read back as asked; `name=v(asked w)` means
the driver kept its own value; `rejected(CODE)` is a name the runtime knows but would not take;
`unknown` is one `HasProperty` denied; `unknown-but-set:` is `HasProperty` denying a name the
write then accepted, which is the one thing that tells us what AMF's `HasProperty` really means.
**If any entry reads `unknown-but-set:`, say so** — that is a finding about the runtime, not
about us.

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
  `--encoder software` forces x264; `--encoder auto` is the shipping chain. Both `probe` and
  `serve` take `--encoder` with the same five values; `serve --software-encode` is the alias for
  `--encoder software` and wins if both are given, because it is also what the desktop UI's
  GPU-encode toggle sets. `auto` is NVENC then Intel then AMF then x264.

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
- `color_space`, `color_primaries` and `color_transfer` must all read **bt709**, and they come
  from *us*, not from AMF. The known AMF driver bug (GPUOpen issues #398, #277, #540:
  `OutColorProfile`, `OutColorPrimaries`, `OutColorTransferChar` and `OutMatrixCoeff` never reach
  the bitstream on any codec) is why `amd/sps.rs` rewrites the SPS VUI on the way out. Which of
  the two happened is in the log, and **this is the line to paste**:

  ```
  AMF SPS/PPS ready (extradata_bytes=...); SPS VUI rewritten for BT.709 full range and
  max_num_reorder_frames=0 (AMF issue #398)
  ```

  or, if this driver has fixed it:

  ```
  AMF SPS/PPS ready (extradata_bytes=...); this driver already writes the colour description and
  the bitstream restriction, so the SPS is left alone
  ```

  Either way the ffprobe fields must be right. If the rewrite ran and they are still absent, the
  rewriter is wrong; if the log says the driver already writes them, AMD have fixed #398 and that
  is worth reporting on its own.
- The **reorder claim** is the other half, and it is what a decoder uses to decide how many
  frames to buffer before showing the first. There is no ffprobe field for it, so parse the SPS:

  ```powershell
  # in another shell, against the same file
  cd src-tauri; cargo test --lib windows_utils::streamer::amd::sps -- --nocapture
  ```

  passes here without hardware; on the box, the assertion that matters is that `ffplay` starts
  showing pictures immediately rather than after a visible pause.
- Then compare against an NVENC capture of the same content on an NVIDIA box:
  `ffprobe -show_entries stream=color_range,color_space,color_primaries,color_transfer` on both,
  and render both in the client page on the *same* device. They should now match. **Record
  whether they do.**
- `ffplay` shows the moving cyan box on a scrolling gradient, not garbage.

Then confirm the first access unit carries SPS and PPS in band:

```powershell
ffprobe -v error -show_packets -of csv out_amd.h264 | Select-Object -First 3
```

---

## 5. Encode latency (10 min)

Live capture, on the real host path:

```powershell
cargo run --release -- serve -v --encoder amd
```

Join from the phone and leave it up for **five minutes**. The per-stage numbers come from the
telemetry summary, which `-v` prints every 5 s per active session and which
`ScreenExtend status --json` reports from another shell:

```
[<client-ip>] telemetry: capture=wgc[...] encoder=amd-amf-ring frames=... fps=...
  capture_delivery=p50/p90/p99/max scale=... encode_queue=... encode=... packetize=...
  socket_write=... capture_to_encode=... capture_to_socket=... | frames_repeated=... idrs_sent=...
```

(The `encode-path latency:` line some older notes mention exists only on the DXGI arm; a WGC
session never prints it.)

Targets, from the NVENC band on comparable hardware, reading the `encode` and
`capture_to_socket` columns:

| Resolution | encode p50 | capture_to_socket p99 |
|---|---|---|
| 1080p60 | single-digit ms | under about 2x the p90 |
| 1440p60 | low teens | under about 2x the p90 |
| 4K60 | teens | under about 2x the p90 |

**Watch the tail, not the median.** A creeping p99 over five minutes is a VBV or buffering
regression and is where latency bugs hide. Paste the first and last telemetry lines of the run.

Also confirm `encoder=amd-amf-ring`. Two other values are possible and both mean something:
`amd-amf-same-adapter` is the inline (non-ring) path, so the ring failed to build and the reason
is in the log; `amd-amf-cpu-bridge` means the capture device was not the AMD adapter at all —
paste the `adapters: capture=… encode=… same_adapter=…` line, because on an all-AMD box that
should not happen.

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
cargo run --release -- probe --bitrate --encoder amd
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
# The output path goes immediately after --encode, or the run writes to the default out.h264 and
# the two files below are the same file.
#
# ULL's default: the backend leaves IntraRefreshMBsNumberPerSlot alone, so the usage's 255 stands.
ScreenExtend probe --encode out_amd_refresh.h264   --encoder amd --intra-refresh on
# Explicitly off: the backend writes IntraRefreshMBsNumberPerSlot = 0, which is the only way to
# turn the usage's default off.
ScreenExtend probe --encode out_amd_norefresh.h264 --encoder amd --intra-refresh off

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

---

## 12. The decoupled ring and LTR repair (10 min)

Both landed `COMPILE-CHECKED` and neither has ever run. They are the two things on this page most
likely to be wrong in an interesting way.

**The ring.** With `serve -v --encoder amd` and a client joined, the log must say:

```
pipeline: live capture ready -- amd-amf-ring RING (K=3) decoupled path (WxH@fps)
```

and the telemetry line must read `encoder=amd-amf-ring`. What it buys is capture/encode overlap:
before it, every AMD frame was encoded *inside* the WGC callback. The number that shows it is
`encode` p50 in the telemetry line — on the NVENC ring it reads **0.00 ms**, because the measured
span is only the submit. If AMF's `encode` p50 is still several milliseconds, the ring is built
but `QueryOutput` is blocking inside the submit, which is expected (AMF has no async submit) and
worth recording as the difference between the two engines.

Also confirm, over five minutes: `frames_dropped_ring` stays 0 (it counts frames dropped for want
of a free slot — if it climbs, the encoder is not keeping up and K=3 is too small),
`frames_dropped_broadcast` stays 0, and `frames_repeated` is roughly two thirds of `frames`
(a static desktop is mostly repeats, and the repeat path is the ring's keepalive here, not
`spawn_repeater`).

**The IDR-on-a-static-desktop case specifically.** Leave the host screen completely still, then
reload the client page. Exactly one IDR must appear (`idrs_sent` increments by 1) within about a
frame. This was a *blocker* on the NVENC ring — 2 IDRs in 30 s before it was fixed — and the AMD
ring uses the same shared `idr_request` atomic through `attach_idr_flag`.

**LTR.** At session start the log says one of:

```
amd-amf-ring: N long-term reference slot(s); a lost frame can be repaired without an IDR
AMF LTR: not enabled — this adapter reports max_refs=1 (need > 1); ...
AMF LTR: not enabled — this runtime has no MaxOfLTRFrames property; ...
AMF LTR: asked for 2 slot(s), the encoder reports 0; using 0
```

**Paste whichever appears.** If LTR is on, the trade is that ULL's rolling intra-refresh is off
(AMF disables it under user-controlled LTR), so step 10's SSIM comparison and this are the two
halves of one decision. Then induce loss — a congested Wi-Fi link, or a client on a busy network
— and compare IDRs per minute with a run where LTR reported 0 slots. The NVENC-side reference is
a repair frame at **23,698 B against a 28,423 B IDR**; the number that matters is how many IDRs a
lossy minute stops producing, not the per-frame size.

---

## What to report back

1. The `AMF AVC caps:` line and the `AMF optional properties:` line, verbatim.
2. The tier from step 3 (ULL / LL / x264) and which message proved it.
3. `has_b_frames`, the `pict_type` set, the IDR count, and `color_range`.
4. First and last telemetry lines of the five-minute run, and the `encoder=` value.
5. Glass-to-glass median and worst, AMF vs x264 on the same box.
6. Anything in the log you did not expect — especially any `unknown-but-set:` entry in the
   optional-properties line, which tells us what AMF's `HasProperty` really means.
7. Step 10's SSIM/PSNR delta and file sizes, and whether the refresh wave is visible.
8. Step 11's retune line, the post-retune encode latency against step 5, and `color_range`.
9. The `AMF SPS/PPS ready` line — whether this driver writes the VUI itself or we rewrote it —
   and whether an AMD stream and an NVENC stream of the same content now look the same in the
   client page.
10. Step 12: the ring line, `encode` p50, `frames_dropped_ring`, the one-IDR-on-a-still-desktop
    result, and the LTR line.
