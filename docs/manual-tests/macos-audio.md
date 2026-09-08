# Manual test: macOS system audio

There is no single audio-capture API across macOS 10.15–current, so ScreenExtend probes at runtime
and takes the first tier that works. This document says how to tell which tier you got, what each
one should measure, and what to do when the answer is wrong.

The tiering is in `macos_utils/audio/mod.rs`; the tiers themselves are `process_tap.rs`,
`sck_audio.rs` and `legacy/`.

## Which tier am I on?

Run the host with logging visible — `ScreenExtend serve -v`, or the Settings log terminal — and
enable system audio on one device. The tier announces itself in one line:

```
audio: capture backend = process_tap        # Core Audio Process Tap
audio: capture backend = screencapturekit   # SCK capturesAudio
audio: capture backend = virtual_device     # the ScreenExtend Audio driver
```

Nothing else prints that string, so this line is the answer. The compatibility report the UI reads
carries the same value as `CompatibilityReport.audio_backend`.

**Selection is by capability, never by version number.** `native_backends()` dlsyms
`AudioHardwareCreateProcessTap` and looks up `CATapDescription`; the SCK probe looks up `SCStream`,
`SCShareableContent`, `SCContentFilter` and `SCStreamConfiguration`. A tier whose classes are
absent is simply not a candidate, and there is no `sw_vers` subprocess anywhere on the path. The
driver tier is appended whenever the driver device is present, so it is a real fallback rather than
a below-13.0-only tier — a 13.x Mac with Screen Recording denied falls through SCK to the driver
instead of failing outright. Each candidate is tried in order and a failure to start moves to the
next:

```
audio: screencapturekit backend failed to start (…); trying next
audio: capture backend = virtual_device
```

If every candidate fails you get `all capable audio backends failed to start`, and if there were no
candidates at all, the message names the driver install instead.

## Tier 1 — Core Audio Process Tap (macOS 14.2+)

Preferred: audio-only, no screen-recording indicator, no dummy video stream.

Lines that prove it is working:

```
audio: capture backend = process_tap
audio: tap aggregate buffer frame size range [<lo>,<hi>] → requested 128, got <n> (<x> ms @ 48 kHz)
audio: output device <id> was <r> Hz; set to 48000 (OSStatus 0)     # only if it was not already
```

and on teardown:

```
audio: restored output device <id> to <r> Hz
```

**Check the restore happens.** Starting the tap sets the user's *default output device* to 48 kHz
because nothing downstream resamples. On a 96 kHz DAC, or a Mac whose owner chose 44.1 kHz for
bit-perfect playback, that is an audible change for every running app — so the prior rate is saved
and put back on stop. Note the rate in Audio MIDI Setup, stream, stop, and confirm it is unchanged.

**Record the buffer-frame-size line.** Nobody — including the AudioCap reference this aggregate is
modelled on — has published what the HAL grants for a *tap* aggregate, so that line is itself the
measurement. Expect the IOProc interval to fall from roughly 10.7 ms (the 512-frame default) to
about 2.7 ms at 128 frames. `dropped_backpressure` in the 2 s summary must stay 0, and the
"produced only silence for 5s" warning must not appear.

Expected: `capture -> packet` p50 under 5 ms, `period_frames` 128–256, `encode p99` well under
1 ms. **Status: COMPILE-CHECKED.** No machine on this project runs 14.2, so no number above has
been observed.

### If it produces only silence

```
audio: process_tap has produced only silence for 5s — if the host IS playing audio, this is the
CATapDescription silent-samples failure mode
```

That is the documented failure where the aggregate's tap-list UID does not match the
`CATapDescription` UUID: the tap returns `noErr` and pure silence. Check the two UIDs agree.

## Tier 2 — ScreenCaptureKit audio (macOS 13.0+)

Fallback. SCK has no audio-only mode, so it rides a 2×2 pixel, 1 fps dummy video stream and needs
the Screen Recording permission.

```
audio: capture backend = screencapturekit
audio: ScreenCaptureKit audio started (48kHz stereo + 2x2 dummy video)
```

Failures are explicit and now bounded at 1.5 s each rather than 5 s, because both blocking bridges
are taken under the `AudioHub` mutex that holds every other session's audio subscribe:

```
audio: screencapturekit backend failed to start (SCShareableContent had no displays
(Screen Recording permission?)); trying next
audio: screencapturekit backend failed to start (SCK startCapture timed out); trying next
```

`SCShareableContent had no displays` is almost always the permission, not the hardware. Grant it in
System Settings → Privacy & Security → Screen Recording and restart the app.

Watch for:

```
audio: ScreenCaptureKit delivered <n> Hz, not the 48000 it was asked for; dropping these blocks
```

`setSampleRate:` is a request. SCK derives its audio from the system mix, and nothing in the macOS
path resamples, so a stream honoured only approximately would encode about 9% fast with A/V drift
accumulating at roughly 80 ms per second. Blocks at the wrong rate are dropped rather than emitted.
**If you ever see that line, say so — it upgrades a speculative finding to a real one.**

Expected: `capture -> packet` p50 under 10 ms (SCK chunks are larger than a tap block).
**Status: COMPILE-CHECKED.**

## Tier 3 — the ScreenExtend Audio driver (macOS 10.15–12.x, and any version as a fallback)

Below 13.0 there is no native system-audio API at all, so this tier ships an `AudioServerPlugIn`
(`src-tauri/macos/ScreenExtendAudio/`), makes it the system default output, and reads the captured
mix back over POSIX shared memory.

**Privacy:** while this tier runs, ScreenExtend *is* the machine's output device. That is why it is
opt-in per device.

```
audio: capture backend = virtual_device
audio(legacy): buffer frame size range [<lo>,<hi>] → requested 128, got 128 (2.67 ms @ 48 kHz)
audio(legacy): capture transport = shm (/screenextend.audio)
audio(legacy): virtual device active (transport=shm, playthrough_dev=<id>,
               volume_keys=device_volume_control)
audio(legacy): crash-recovery agent loaded
```

`transport=shm` is the fast path. `transport=hal_input` means the driver could not create its
shared segment and the host fell back to reading the device's input stream — check the driver's own
log (below). The fallback now validates the delivered ASBD and refuses a rate that is not 48 kHz
rather than reinterpreting whatever arrives as stereo float.

On a clean stop:

```
audio(legacy): stopped; default output restored
audio(legacy): crash-recovery agent removed
```

Both lines matter. The first says the user's real output device is back. The second says the
LaunchAgent that exists to restore it after a *crash* has been booted out — it is installed per
session and removed on every path that ends its job, so `launchctl list | grep screenextend` should
find nothing once audio is off.

Every 10 s, if the local monitoring has starved:

```
audio(legacy): playthrough underruns <n> (+<d>), setpoint raised by <b> frames (<x> ms)
```

The setpoint is the host's own monitoring latency and starts at three of the output device's IO
periods; it is only ever raised by a measured underrun. A `setpoint raised by` that keeps climbing
past a few hundred frames means this machine needs a higher floor.

### Verifying the driver directly

```sh
cd src-tauri/macos/ScreenExtendAudio
clang tools/shmcheck.c -o /tmp/shmcheck -lm
/tmp/shmcheck            # one snapshot
/tmp/shmcheck watch      # poll, printing writePos delta and RMS
```

A healthy segment, with the host attached and audio playing:

```
open O_RDWR (what the host needs): granted
header magic=0x31414553 version=2 rate=48000 ch=2 fmt=1 cap=131072 hdr=128
writePos=<climbing> readPos=<tracking it> backlog=<small> gen=1 overruns=0 rms=<nonzero> NON-SILENT
anchor: pos=<n> hostTime=<mach ticks>
```

- `open O_RDWR … granted` is the umask check. The driver clears the umask across `shm_open` so the
  segment is 0666 regardless of what `coreaudiod` runs with; `denied` means the host cannot map it
  and will take the HAL-input path.
- `version=2` is the layout with the capture-time anchor and the reader's published position. A
  layout-1 segment from an older driver is unlinked and recreated at load.
- `backlog` should stay small and `overruns` at 0. A climbing backlog means the host reader is
  behind; climbing overruns mean it lost samples.
- `anchor: hostTime=0` means the driver has not published a capture time — packets then fall back
  to the encoder's own clock and A/V sync suffers.

The driver's own log:

```sh
log stream --predicate 'subsystem == "app.screenextend.desktop.audio"'
```

It reports the segment open result with `errno`, the resolved mode, the generation, and every
device build. Silence there while `transport=hal_input` is reported means the plug-in never loaded
at all — check that it is signed, or that SIP is disabled.

## The one-command measurement: `probe --audio`

`probe --audio` is the shared instrument — the same one W8 built for Windows — so its numbers are
directly comparable across platforms. It subscribes to the real audio hub for ten seconds and
prints the whole picture. Something must be playing, or it correctly reports no packets:

```sh
(ScreenExtend probe --audio > /tmp/probe.out 2>&1 &) ; sleep 3
say "one two three four five six seven eight nine ten"
sleep 11 ; cat /tmp/probe.out
```

Three lines are worth reading first:

```
opus: libopus … application=2051 bitrate=128000 complexity=5 signal=3002 vbr=1 vbr_constraint=1
      lsb_depth=24 prediction_disabled=0 lookahead=120 samples (2.50 ms), frame 5 ms
audio: capture backend = virtual_device
capture -> subscriber   p50/p90/p99/max = 1.84/3.67/5.24/6.76 ms
```

The first must match Windows field for field, `lookahead=120` included — macOS loads a *different*
libopus build, and that number is what the client uses to align playout. The second is the tier.
The third is the figure to compare: on Windows 11 the endpoint-loopback tier measures
p50 9.44 / p99 9.44 ms against this tier's 1.84 / 5.24, which is what a 2.67 ms IO cycle buys
against a 10 ms engine period.

Expect roughly 50 packets/s rather than 200 while a phrase plays: the virtual device runs no IO
cycle during host silence, so the stream genuinely pauses between sounds and costs nothing on the
wire. `seq gaps` must be 0; `discontinuity` counts re-seats across those pauses and 1 per gap is
correct.

### Measured on the dev box — **VERIFIED**

macOS 10.15.8, Intel, SIP disabled, driver built from the tree with `scripts/build.sh` and loaded
by a `coreaudiod` kickstart (no reboot). Three 10 s runs through the shipped `start_capture()`
path:

| | p50 | p90 | p99 |
|---|---|---|---|
| capture → Opus packet | −0.57 ms | 3.59 ms | 4.54 ms |
| Opus encode | 0.09–0.12 ms | | 0.16–0.27 ms |

- `capture_ns` deltas: **0 of 3277** were not exactly 5.000 ms. Zero regressions, zero sequence gaps.
- IO cycle: 128 frames (2.67 ms), requested and granted.
- 2,083,840 samples through the shm ring with `readPos` tracking `writePos`, backlog 0, overruns 0.

`p50` is negative on purpose: the driver stamps the mix with `mach_absolute_time()` in
`OnProcessMixedOutput`, which is when the mix exists and before it would have been heard, so a
packet reaches the encoder ahead of its own audio's playout. Unlike the Windows loopback case —
where the endpoint timestamp is a play cursor about 15 ms in the future and has to be clamped —
this is under one IO cycle and in the right direction.

A fourth run with a **3 s silent gap** between two phrases: exactly 1 delta of 1684 was not
5.000 ms (the re-seat), zero regressions, zero sequence gaps. The virtual device runs no IO cycle
while nothing is playing, so the packet stream genuinely pauses and costs nothing on the wire —
which is as close to DTX as this pipeline gets, since Opus in `RESTRICTED_LOWDELAY` is CELT-only
and cannot do it.

Reproduce with, from `src-tauri/`:

```sh
cargo test --lib legacy_live -- --ignored --nocapture
```

Both tests skip cleanly if the driver is not loaded. They take over the default output device and
restore it on the way out.

## What is still unverified

| Claim | Status |
|---|---|
| Process Tap tier starts, restores the device rate, and honours 128 frames | COMPILE-CHECKED — needs a 14.2+ Mac |
| SCK audio tier starts and delivers 48 kHz | COMPILE-CHECKED — needs a 13.0+ Mac |
| Driver loads **signed and notarized, with SIP on** | UNVERIFIED — no Apple identity on either dev machine; see `macos-audio-driver-signing.md` |
| Legacy tier capture, timing, shm transport, teardown | **VERIFIED** on 10.15.8 |
| Volume-key event-tap fallback | COMPILE-CHECKED — opt-in, needs Accessibility |
| Playthrough click-to-sound latency | COMPILE-CHECKED — needs a second machine to record with |
