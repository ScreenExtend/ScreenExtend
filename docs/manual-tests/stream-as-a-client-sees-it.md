# Manual test: the stream as a client actually sees it

**Why this exists:** every latency number in `docs/RESULTS.md` above the "User-facing stream
quality" section came from `ScreenExtend probe`, which paints a full-frame animation into a
borderless window and never involves a browser. On that content every frame is a changed frame,
so the probe cannot see a pipeline that drops the frames the compositor calls unchanged; and with
no client in it, it cannot see a page that demoted itself off the WebCodecs path, or a decoder
asking for a keyframe it never gets. Six defects lived in exactly that blind spot
(`docs/OPTIMIZATIONS.md` rows 325-332) while the probe tables got faster.

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
6. **The pointer is drawn, and the host is drawing it.** `host.log` carries one line per session:
   `cursor: composited by the host on N ring slots; WGC asked for the desktop without a pointer.
   display WxH at X,Y (hmonitor …) -> encode WxH, cursor dpi factor D`. The rectangle must be the
   *virtual* display's, not `0,0` — a virtual display sits at a negative x beside the panel, and
   `0,0` means `GetMonitorInfoW` failed and the pointer will be tested against the wrong screen.
   `cursor_frames` in the host summary must be non-zero once the pointer has moved, and any
   `[cursor] no pointer drawn:` line names which test rejected it and where the pointer was.
   Where the overlay could not be built the line instead says WGC keeps the pointer, which is the
   old behaviour and not a failure.
7. **Look at the picture.** Extract a frame from the recording and check there is a pointer in it,
   the right shape, with its tip where the sweep should have put it:
   ```powershell
   ffmpeg -y -ss 18 -i <run>\client.mkv -frames:v 1 -vf "crop=640:420:60:140,scale=1280:840" f.png
   ```
   Nothing else in this test can catch a pointer that is not being drawn: the first version of the
   host-composited path passed every host-side counter while drawing nothing, because its
   `MonitorFromPoint` test never matched.

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

Whether the pointer *looks* right to a person. It can tell you the pointer is being drawn, how
often it moves and how long the longest stall was; it cannot tell you the hotspot is a pixel out,
or that the sprite is the wrong size on a display at 200 %. Check 7 is the closest this gets, and
it is a still frame.

It also cannot tell you anything about a Windows build older than this box's. The compositor's
willingness to produce a frame for pointer-only motion is the whole question the host-composited
pointer exists to remove, and it is a per-build behaviour: 26200 delivers ~50 fps for it, and
nothing here says what 19041 does. `guides/windows-legacy.md` owns that.

### A warning from this test's own history

Both of this harness's oracles were wrong at once, and both looked fine:

- `SendInput` returned 0 for every call because the `INPUT` struct was 48 bytes instead of 40, so
  the **cursor scene had a stationary cursor** and produced a plausible measurement of an entirely
  different scene. The conclusion drawn from it — that WGC delivers 2-10 frames a second for
  pointer-only motion — reached two other documents before anyone moved the pointer.
- The recording's crop options were output options, so it recorded the whole desktop and counted
  changes that had nothing to do with the stream.

Both are fixed and the sweep is now self-tested. The lesson is the reason check 7 exists: an
oracle that cannot fail loudly will eventually agree with you about something that is not
happening.
