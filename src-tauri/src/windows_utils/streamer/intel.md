# The Intel Quick Sync (oneVPL) H.264 backend

The predecessor of this file was a pre-implementation design note, written before
`intel/encoder.rs` existed and deleted by the commit that implemented it. This one is
deliberately a different kind of document: a **map**, not a specification. It says what the two
Intel paths are, what decides between them, and where the reasoning for each individual setting
lives — which is in the code, next to the setting, where it cannot drift out of step with it.

`amd.md` is still a specification because the AMD backend has never encoded a frame. This one
describes something that runs.

## The two paths

`build_backend` (`pipeline.rs`) produces one of two Intel backends, and the difference is which
adapter holds the captured texture.

**`Backend::Intel` — same adapter.** The captured texture is already on the Intel device, so it
is copied into a oneVPL VPP input surface, and one `VideoProcessorBlt`-equivalent VPP pass does
the scale *and* the BGRA→NV12 conversion before the encode. This is why `wants_prescale()`
returns false for it: adding a separate scaler would be a second pass doing work the VPP is
already doing. Per-frame stage medians on Intel UHD Graphics, 1920x1080 → 1280x720
(`intel_quicksync_fused_downscale` prints them): get_surface 2 us, copy 136 us, VPP submit 64 us.

**`Backend::IntelCpu` — cross adapter.** The captured texture is on some other GPU. It cannot be
shared: `OpenSharedResource1` on an NT handle requires both devices on the same adapter, so the
shared-texture trick that lets NVENC take a cross-adapter capture texture has no equivalent here,
and the frame has to come back through system memory. What makes that as cheap as it can be:

- W3a's adapter policy puts capture on the encoder's adapter where it can, which is why this
  development box now reports `intel-same-adapter` rather than `intel-cpu-bridge`;
- `Backend::cpu_bridge_format()` asks the scaler for **NV12**, so the readback, the memcpy and the
  upload carry 1.5 bytes a pixel instead of 4;
- `encode_nv12_padded` uploads straight into the encode surface and skips the VPP pass entirely,
  because the scaler has already done that conversion. Measured 3766 → 2820 us per frame at
  640x360 (`intel_nv12_input_matches_the_bgra_path`);
- `Auto` tries NVENC first, so this path is reached only when NVENC has already failed on a
  machine that has both.

There is a third shape inside `Backend::Intel`: when the capture device is Intel but lacks
`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`, `InputBridge` creates a dedicated Intel device and passes
frames over a same-adapter keyed-mutex shared texture. That bridge drives the *capture* device's
immediate context from the encode thread, which is why `set_multithread_protected` is called on
both contexts and its failure is an error rather than a discarded `let _ =`.

## What decides each setting

Every non-obvious parameter carries its reasoning as a comment at the assignment. The ones worth
knowing exist:

| Area | Where | The short version |
|---|---|---|
| Rate control | `init_encode` | `MFX_RATECONTROL_VCM`, not CBR. `LowDelayBRC` and `MaxFrameSizeI`/`P` are defined for VBR/QVBR/VCM only, so under CBR all three were no-ops. `ENCODE_Query` decides; refusal falls back to CBR **and clears the three dead fields**. |
| VBV | `vbv_kb` | One frame of bits. It used to be one uncompressed BGRA frame — 8160 KB at 1920x1088, about 6.5 s of CPB at 10 Mbps. The output *allocation* is a separate number. |
| Slices | `SLICE_BYTE_TARGET`, `SliceMode` | `MaxSliceSize` at one RTP payload where the runtime honours it (it does on Intel UHD Graphics with `LowPower = ON`), else the height heuristic. Loss resilience, not latency. |
| Reset | `set_bitrate` | The three ext buffers are re-attached. oneVPL reverts every ext buffer omitted at `Reset` to its default, so the first bitrate change used to undo the whole low-latency posture. |
| Session creation | `create_session` | Serialised process-wide and retried. Implementation enumeration is dispatcher-wide and two at once against one iGPU makes the loser see `MFX_ERR_NOT_FOUND`. |
| Dispatcher | `intel_sys::open_dispatcher` | OS path first. The driver's `libvpl.dll` is matched to the runtime the same package installed; an app-side copy must never shadow it. Nothing is shipped — `resources/PROVENANCE.md` says why. |
| Busy retries | `DeviceBusyBackoff` | Spin, then yield, then 200 us sleeps, under a 50 ms wall-clock budget. `sleep(1ms)` on Windows can be 15.6 ms. |
| Per-frame flush | `per_frame_flush_enabled` | Kept, and measured: removing it cost 420 us at p50 because the pipeline is synchronous per frame and the early submit buys overlap. |
| Warnings | `check`, `check_no_software_fallback` | Positive `mfxStatus` values are logged by name. `MFX_WRN_PARTIAL_ACCELERATION` at Init is a hard failure — it means the CPU is encoding under a log line that says Quick Sync. |

## Verifying it

`cargo test --lib intel -- --nocapture` on a machine with an Intel GPU. Set `SE_REQUIRE_HW=1` so a
skip becomes a failure rather than a green tick over a path that never ran. The tests print the
effective configuration, the slice-size distribution, the per-stage split and encode percentiles;
`intel_layout.rs` pins the oneVPL ABI, including the rate-control enum, whose gaps are reserved
values that cannot be inferred from their order.

The one thing none of this proves is that the *runtime* agrees with the ABI — only running the
encoder does that, which is what the functional tests are for.
