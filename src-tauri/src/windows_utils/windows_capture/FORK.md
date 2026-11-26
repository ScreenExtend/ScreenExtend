# `windows_capture`: vendored fork

This directory is a fork of the [`windows-capture`](https://crates.io/crates/windows-capture)
crate, cut down to the one thing ScreenExtend uses it for: capturing a monitor into a D3D11
texture. This file records where it came from, what diverges, and what was deleted.

## Upstream

| | |
|---|---|
| Crate | `windows-capture` |
| Version | 2.0.0 |
| crates.io checksum | `0f9460c7b82f6b7d314d85b45610481f26a1159a42dadf1e13a329041553e060` |
| Recover the pin | `git show ad598b2^:src-tauri/Cargo.lock` |

Vendored in `ad598b2` (2026-06-30, *refactor(windows): vendor windows-capture crate in-tree*),
which also deleted `src-tauri/vendor/windows-capture.patch`, until then the only record of
divergence. Upstream's `src/lib.rs` became this directory's `mod.rs`; it was byte-identical after
normalization at the time of the move.

Two later commits are worth knowing about when reading a diff:

- `37c0f82` (2026-08-01) rustfmt'd the whole directory to `max_width = 100` and desugared
  let-chains (upstream is edition 2024, this crate is 2021). No semantic change; it touches every
  file and will dominate a naive diff.
- `5ba1183` (2026-08-28) changed exactly one line: the frame-pool buffer count passed to
  `Direct3D11CaptureFramePool::Create`, 1 → 3. The count is now supplied by the handler
  (see below), so that literal is gone again.

## Why the fork exists

`d3d11.rs::create_d3d_device` requests `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` alongside
`D3D11_CREATE_DEVICE_BGRA_SUPPORT`, falling back to BGRA-only if the driver refuses
(the retry at the end of `create_d3d_device`). Upstream asks for BGRA only. That flag is what
allows the capture device to
be shared with NVENC/oneVPL and is what `Scaler`'s `ID3D11VideoDevice` cast requires; without it
the whole same-adapter encode path collapses to a CPU bridge. This was the sole content of the
deleted `windows-capture.patch`, and it is still the load-bearing divergence. The fallback is now
logged via `teprintln!` rather than swallowed silently.

## Upstream bugs fixed here

Sites are named by symbol rather than by line number; line numbers went stale within a month.

| # | Site | What upstream does wrong |
|---|---|---|
| 1 | `capture.rs` `run_message_loop` | `while GetMessageW(..).as_bool()` treats the `-1` error return as `true`, so a failing message pump spins forever. The fork matches `-1` / `0` / `_`. |
| 2 | `frame.rs`, and the now-deleted `dxgi_duplication_api.rs` | Leaves the staging texture mapped and leaks the COM object. The fork unmaps on drop via the RAII `MappedStagingTexture` (`d3d11.rs`, its `Drop`). |
| 3 | `frame.rs` `FrameBuffer::has_padding` | Hardcodes 4 bytes per pixel, wrong for `Rgba16F` (8 bpp). The fork uses the real `bytes_per_pixel()`. |
| 4 | `frame.rs` `as_nopadding_buffer` | Tests `buffer.capacity() < frame_size`, so a `Vec` with adequate capacity but a short `len()` is handed back partially uninitialized. The fork tests `len()`. |
| 5 | `capture.rs` `CaptureControl::stop` | Does not handle `GetThreadId == 0`, and does not yield on `ERROR_INVALID_THREAD_ID` in the `PostThreadMessageW` retry loop, which can spin against a thread that has not finished registering its queue. |
| 6 | `window.rs` (deleted) | Enumerated with `EnumChildWindows(GetDesktopWindow())` rather than `EnumWindows`, and cast a possibly-negative title-bar height to `u32` without clamping. |
| 7 | `capture.rs` `impl Drop for CaptureControl` | Upstream has no `Drop`: dropping a control without `stop()` detaches the capture thread, which then waits in `GetMessageW` forever holding the session, the pool, the device and the handler (here an NVENC session and its threads). The fork's `Drop` raises the halt flag and posts `WM_QUIT`, retrying for at most 50 ms while the thread's queue does not exist yet, and **never joins**, because a control is dropped on whatever thread owns it, including a tokio worker. `into_thread_handle` now takes `mut self` and `take()`s the handle, so a caller that takes the handle keeps the capture running. |
| 8 | `graphics_capture_api.rs` `GraphicsCaptureApi::new` | A fallible call between the delegate registrations and the final `Ok(Self{..})` (a setter that exists and fails) returned without `cleanup()`, leaving both delegates, and the handler they hold, alive. The fork builds `Self` before the first registration, so any `?` drops it and `cleanup()` runs; the event tokens are `Option`s so only registered handlers are removed. The `FrameArrived` closure also no longer captures a clone of the pool (a pool -> delegate -> pool cycle only `RemoveFrameArrived` could break): it uses the event's sender for `Recreate`. |
| 9 | `graphics_capture_api.rs` both delegates | `PostThreadMessageW(..)?` returned a failed post to WinRT, which ignores a delegate's result, so the halt flag was set and the loop never ended. `quit_capture_thread` uses `PostQuitMessage` on the capture thread itself (it cannot fail) and logs a failed post from any other thread. |
| 10 | `graphics_capture_api.rs` `FrameArrived` closure | `ContentSize()?`, `Surface()?`, the DXGI-interface cast, `GetInterface()?` and `Recreate(..)?` returned their errors to WinRT, which ignores them: no log, no counter, no halt, and a `Recreate` that keeps failing failed on every frame forever. They now go to the handler's `on_frame_pool_error` hook (see below) with a count of consecutive failures. |

## Deliberate divergences (not bug fixes)

- `d3d11.rs` `StagingTexture::from_raw_checked` requires **both** `D3D11_CPU_ACCESS_READ`
  and `D3D11_CPU_ACCESS_WRITE`; upstream accepts either. `MappedStagingTexture` maps
  `D3D11_MAP_READ_WRITE`, so read-only would fault at map time instead of at validation.
- Many `unwrap`/`expect`/`panic!` sites replaced with typed errors: `winrt.rs`, `capture.rs`,
  `d3d11.rs` (`Error::UnexpectedNullResult`, for the Win32 pattern of returning `S_OK` without
  populating an out-param).
- `graphics_capture_api.rs` de-duplicated `stop_capture` and `Drop` into a shared `cleanup()`;
  upstream had two copies that had already drifted.

## Other divergences, by file

### `graphics_capture_api.rs`

- **One WGC predicate, and the `UniversalApiContract 8` conjunct is gone.** `wgc_supported()` is
  `GraphicsCaptureSession::IsSupported()` alone; `GraphicsCaptureApi::is_supported()` delegates to
  it, `wgc_features()` gates on it, and `windows_utils::compatibility` calls it rather than keeping
  a second copy. Upstream ANDed in
  `IsApiContractPresentByMajor("Windows.Foundation.UniversalApiContract", 8)` (Windows 10 1809),
  and the compatibility probe had dropped that conjunct, so on a build where the gate binds the
  report claimed WGC while capture refused to start and every optional knob read absent. The
  contract check is the half that went, because the capture path uses no API newer than the
  contract-6 set `IsSupported()` itself belongs to (`Create`, `CreateCaptureSession`,
  `item.Closed`, `FrameArrived`, `item.Size`); everything above that is probed individually below.
  `windows_utils::compatibility`'s
  `the_report_and_the_capture_path_share_one_wgc_predicate` fails if a term is ever re-added to
  one side only.
- **`WgcFeatures` + `wgc_features()`**. Every optional `GraphicsCaptureSession` knob is probed
  once through `ApiInformation` and cached in a `OnceLock`: `IsCursorCaptureEnabled` (19041),
  `IsBorderRequired` (20348), `IncludeSecondaryWindows` (22000), `MinUpdateInterval` and
  `DirtyRegionMode` (both 26100), plus `Direct3D11CaptureFramePool::CreateFreeThreaded`
  (17763).
- **An absent property is skipped, never fatal** (the setter block at the end of
  `GraphicsCaptureApi::new`). Upstream, and earlier versions of this fork, returned an error when a
  setter was unavailable. Since `MinUpdateInterval` and `DirtyRegionMode` only exist on 24H2,
  treating either as required silently routed every Windows 10 and pre-24H2 Windows 11 host onto
  the DXGI fallback.
- **The settings are read back once per session** after the setters, and logged as
  `wgc: session settings in effect: pool=.. buffers=.. cursor=.. border_required=..
  min_update_interval=.. dirty_region_mode=..`. Upstream never reads them. `IsBorderRequired`
  reports what the session holds, not whether the compositor still draws a border; whether an
  unpackaged process needs `GraphicsCaptureAccess.RequestAccessAsync(Borderless)` consent for it
  is not settled, and nothing here asks for it.
- **The pool drains to its newest frame** (the `TryGetNextFrame` loop in the `FrameArrived`
  closure). One `FrameArrived` event per frame only holds while the consumer keeps up; once it does
  not, each event replays a backlog up to `buffers - 1` frame periods old. Discarded frames are
  counted into an `Arc<AtomicU32>` the handler supplies.
- **`Recreate` uses the same buffer count as `Create`** (the size check in the `FrameArrived`
  closure). Upstream passes 1, so the first content-size change (which on this host is a client
  resolution change on its virtual display) quietly re-serializes capture against encode for the
  rest of the session. The handler is not told about the change; the next frame it receives is a
  different size, which is what `LiveCapture` checks for.
- **The buffer count comes from the handler**, clamped to 1..=8.
- **The pool can be free-threaded** when the handler asks (`frame_pool_free_threaded`) and the
  build has `CreateFreeThreaded`; `Create` otherwise, as upstream. The read-back log line names
  which one a session got.

### `capture.rs`

- `GraphicsCaptureApiHandler::frame_pool_buffers` and `stale_frame_counter`, both defaulted (2,
  and `None`), read once at start in both `start` and `start_free_threaded` and passed to
  `GraphicsCaptureApi::new`.
- `GraphicsCaptureApiHandler::cursor_capture_override`, defaulted `None`, read in the same block
  as the two above and used in place of `Settings::cursor_capture_settings` when it is `Some`.
  It exists because the right value is not knowable until the handler has been constructed: our
  handler draws the pointer itself when it can build a `CursorOverlay` on the encoder's ring
  slots, and only then needs `WithoutCursor`. Setting it in `Settings` would fix the value
  before it is known whether the overlay can be built, and an overlay that then
  failed would leave a stream with no pointer at all. (The overlay is off by default since WGC's
  own pointer measured faster, 85 fps against 60, so this override is `None` on a default host.)
- `GraphicsCaptureApiHandler::frame_pool_free_threaded`, defaulted `false`, read in the same
  block: whether the pool is created with `CreateFreeThreaded`.
- `GraphicsCaptureApiHandler::on_frame_pool_error(&error, consecutive)`, defaulted to log the
  first failure of a run and carry on. Called from the `FrameArrived` closure when the pool
  signalled a frame it could not hand over; an `Err` halts the capture like an
  `on_frame_arrived` error.
- **The item is converted before the handler is built**, in both `start` and
  `start_free_threaded`. Upstream builds the handler first, and a monitor handle that went stale
  then cost a whole handler (here an NVENC session, its ring thread and the GPU clock hold) per
  stale-monitor retry before `ItemConvertFailed` was even reached.
- **The capture thread is named `wgc-capture`** (`thread::Builder`, a spawn failure maps to
  `FailedToStartCaptureThread`), so a WPR/ETW trace can attribute it.
- `Context` now carries an `AdapterInfo`, so the handler can read which GPU it got.
- Both start paths also convert the capture item before they call `create_d3d_device`, so a
  monitor handle that went stale costs no device either: each NVIDIA device that allocates
  leaves one more driver handle in the process, and a stale-monitor retry used to create one
  for nothing.

### `d3d11.rs`

- `GpuPreference`: `Default` / `HighPerformance` / `MinimumPower` / `Vendor(u32)`.
  WGC, unlike DXGI duplication, can capture any monitor from a device on any adapter, so the
  capture device can be placed on the *encoder's* adapter; NT-handle keyed-mutex sharing is
  same-adapter only.
- `AdapterInfo` and `device_adapter_info`: description, PCI vendor id,
  flattened LUID, and a software-rasteriser flag (`DXGI_ADAPTER_FLAG_SOFTWARE`, or Microsoft's own
  vendor id `0x1414`).
- `enumerate_adapters` orders adapters via
  `IDXGIFactory6::EnumAdapterByGpuPreference`, obtained by `QueryInterface` rather than by build
  number, and falls back to `EnumAdapters1` ordering where the interface is absent.
  `resolve_adapter` skips software adapters; if that is all there is, the default path runs.
- `create_d3d_device` takes a `GpuPreference` and returns the `AdapterInfo` it
  actually landed on. With an explicit adapter the driver type must be `D3D_DRIVER_TYPE_UNKNOWN`;
  `HARDWARE` plus an adapter is an invalid-argument error, not a hint.

### `frame.rs`

- `Frame::buffer_with(&mut StagingTexture)` mirrors what the deleted DXGI wrapper
  already had. `Frame::buffer` allocates and destroys a full-resolution staging texture per call:
  a kernel-mode allocation on the capture thread, per frame, for every CPU-bridge encode. The
  caller now owns the texture and recreates it when the content size changes.
- `Frame::has_dirty_regions()` answers `DirtyRegions().Size() != 0` without building the `Vec`
  `dirty_regions()` returns (one WinRT call per region plus an allocation on every changed
  frame); the per-frame unchanged-frame gate only needs emptiness.

### `settings.rs`

- `Settings::with_gpu_preference`.
- `GraphicsCaptureItemType` collapsed to the `Monitor` variant; see below.

### `winrt.rs`

- **`WinRT::new()` checks the apartment before it initializes.** Upstream calls
  `RoInitialize(RO_INIT_MULTITHREADED)` unconditionally. `Handler::start` runs on the *caller's*
  thread, and in this app that caller can be one Tauri already put in an STA, where an
  unconditional multithreaded initialize returns `RPC_E_CHANGED_MODE` and takes the whole capture
  down with `FailedToInitWinRT`. It now goes through
  `windows_utils::com::apartment_of_this_thread()` and initializes only a thread with no apartment
  of its own, `RoUninitialize`ing only what it took. `start_free_threaded` (the path the live
  pipeline uses) spawns its own thread and is unchanged.
- **`keep_process_com_alive()` on entry.** This type's `CoIncrementMTAUsage` is balanced on drop,
  so on a host where capture is the only COM user the last stop drops the process's last COM
  reference, while `windows-core`'s `FactoryCache` keeps WGC's agile activation factories in
  process-wide raw pointers that outlive it, and the next capture start reads a dead factory. One
  unmatched `CoIncrementMTAUsage`, taken once for the process, keeps COM initialized for as long
  as the process runs.

## Removed

| What | Size | Why |
|---|---|---|
| `dxgi_duplication_api.rs` | ~951 lines | Nothing imported it. ScreenExtend has its own `windows_utils/streamer/dxgi/`, which unlike the vendored one reads `AccumulatedFrames`/`LastPresentTime` and calls `GetFrameDirtyRects`/`GetFrameMoveRects`. The vendored one also had two `unreachable!()` arms that are reachable on an HDR desktop. |
| `window.rs` | ~402 lines | ScreenExtend captures only the virtual display it created: no window capture. |
| `graphics_capture_picker.rs` | ~171 lines | No system picker either. |
| The Media Foundation `VideoEncoder` half of `encoder.rs` | ~1,290 of the file's 1,481 lines | `MediaStreamSource` + `MediaTranscoder` writing MP4/HEVC. ScreenExtend encodes with NVENC/oneVPL/AMF/x264 and packetizes itself; it never asked Windows to mux a file. `ImageEncoder` is kept for `Frame::save_as_image` on the `--probe-capture` path. 165 lines remain. |
| Crate-root lints | n/a | `#![warn(clippy::nursery)]`, `clippy::cargo` and `clippy::multiple_crate_versions` check the *whole* dependency graph of a large Tauri app and are unstable across toolchains, while the repo's gate is `cargo clippy --all-targets -- -D warnings`. `#![warn(missing_docs)]` is kept (`mod.rs`). |

`GraphicsCaptureItemType` lost its `Window` and `Unknown` variants along with the first two files,
which is why every match on it in this directory is now an irrefutable `let`.

## Re-deriving the diff

```sh
curl -LO https://static.crates.io/crates/windows-capture/windows-capture-2.0.0.crate
tar xzf windows-capture-2.0.0.crate            # -> windows-capture-2.0.0/src/

printf 'max_width = 100\n' > /tmp/rustfmt.toml
rustfmt --config-path /tmp/rustfmt.toml --edition 2021 windows-capture-2.0.0/src/*.rs
rustfmt --config-path /tmp/rustfmt.toml --edition 2021 \
        src-tauri/src/windows_utils/windows_capture/*.rs   # already formatted; no-op

# strip CRs on both sides before comparing
for f in windows-capture-2.0.0/src/*.rs src-tauri/src/windows_utils/windows_capture/*.rs; do
  tr -d '\r' < "$f" > "$f.lf" && mv "$f.lf" "$f"
done

diff -u windows-capture-2.0.0/src/lib.rs src-tauri/src/windows_utils/windows_capture/mod.rs
for f in capture d3d11 encoder frame graphics_capture_api monitor settings winrt; do
  diff -u "windows-capture-2.0.0/src/$f.rs" \
          "src-tauri/src/windows_utils/windows_capture/$f.rs"
done
```

Every hunk that survives should be explained by an entry above. One that is not is either a new
divergence that belongs in this file, or a mistake.
