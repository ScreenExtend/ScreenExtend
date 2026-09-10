# Manual test: the stream as a client actually sees it

**Why this exists:** every latency number in `docs/RESULTS.md` above the "User-facing stream
quality" section came from `ScreenExtend probe`, which paints a full-frame animation into a
borderless window and never involves a browser. On that content every frame is a changed frame,
so the probe cannot see a pipeline that drops the frames the compositor calls unchanged; and with
no client in it, it cannot see a page that demoted itself off the WebCodecs path, or a decoder
asking for a keyframe it never gets. Six defects lived in exactly that blind spot
(`docs/OPTIMIZATIONS.md` rows 324-331) while the probe tables got faster.

`scripts/measure-stream.ps1` closes it, and most of this test is running it. The steps below are
the parts it cannot check for you.

## What you need

- A Windows host with the bundled virtual-display driver installed, and Chrome.
- `ffmpeg` on `PATH` for the freeze count. Without it the run still produces every client number;
  it just skips the recording.
- Nothing else heavy running. The harness's own browser windows are already on the machine being
  measured; see "Reading the numbers".

## Run it

```powershell
# the current build, pointer sweeping an otherwise empty virtual display
scripts\measure-stream.ps1 -Binary <path>\ScreenExtend.exe -Label after -Scene cursor -Seconds 30

# the same, over a full-frame 60 fps animation on the virtual display
scripts\measure-stream.ps1 -Binary <path>\ScreenExtend.exe -Label after -Scene motion -Seconds 30
```

Each run writes `%TEMP%\se-measure\<label>-<scene>-<stamp>\` containing `summary.json` (every
per-second sample), the host's own log, the client's console, and the recording. Take the
**median of three**; single runs are not comparable.

## What must be true

1. **The display the host builds matches the device.** In `host.log`:
   `virtual display created (id=0, WxH@R)`. `W`x`H` must be the client's screen size times its
   `devicePixelRatio`, and `R` must be at most `DEFAULT_MAX_AUTO_REFRESH` (60) unless somebody
   set a higher rate for that device in Devices. A 3840x2160@360 display for a 1920x1080 client
   is the failure this test was written after.
2. **The client stays on the fast video path.** `videoPath 1` and `renderTier 1` or `2`, and the
   host's summary line reads `client video=webcodecs+canvas`. `videoPath 3` with
   `demoted_to_video_element` in the degradation list means the page gave up on WebCodecs — check
   the client console for `no WebCodecs frame within 5s`, which means it never got a keyframe
   after installing its transform.
3. **There is no keyframe storm.** `keyframe_requests` and `idrs_sent` in the host summary line
   should be single digits over 30 s on an idle desktop. Tens or hundreds means something is
   asking continuously; that is the client's backlog path or a repair frame it cannot decode.
4. **The host's own desktop does not stutter.** `desktop smoothness` should report the panel's
   rate with `0 hitches/min` and a p99 within a millisecond or two of the frame period, and it
   should not be materially worse than the `desktop, host up but idle` line above it.
5. **Nothing skips a frame carrying the pointer.** `frames_skipped_unchanged` must be **0** with
   `autoFrameRate` off, which is the default. If it is not, the WGC dirty-region skip is running
   when it should not be.

## Reading the numbers

- **Clear the saved device row, or know that you did not.** A row saved from an earlier session
  pins the geometry and the refresh rate for every later join from that address; the harness
  clears `devices` in `config.json` for the run and restores it afterwards. Measuring against a
  stale row measures the row.
- **`painted frames/s` and `changed frames/s` are different questions.** The first is how often
  the client painted anything; the second is how often the picture actually changed. On a still
  virtual display most of what the host emits is a keepalive repeat, so the first can be an order
  of magnitude higher than the second and the pointer can still look frozen.
- **The harness perturbs visibility.** It opens a second small window to sample the desktop's
  animation cadence, and Chrome treats a window that is not visible differently: `wake_lock_refused`
  in the degradation list, alongside a demotion, usually means the join page was not the visible
  window rather than anything about the build.

## What this cannot tell you

The pointer's own smoothness on an **indirect** display. WGC does not deliver a frame for
pointer-only motion there — measured at 2-10 real frames a second with the pointer sweeping
continuously, on both the current build and `271aa01` — so the changed-frame rate is bounded by
something no host-side pacing can lift. The fix is to stop asking WGC to composite the pointer
and to draw it from `GetCursorInfo` into the ring slot, reusing
`windows_utils/streamer/dxgi/cursor.rs`. Until that lands, this test measures everything around
the pointer rather than the pointer.
