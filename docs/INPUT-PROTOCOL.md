# The input wire protocol

The client → host remote-control protocol, as `src-tauri/src/streamer/input/protocol.rs` parses
it and `src-tauri/src/streamer/static/input.js` builds it. Everything is little-endian with
fixed offsets; there is no framing beyond the DataChannel message boundary, because SCTP
preserves them.

Three channels carry it, opened by the client (`input.js`):

| Channel | Options | Carries |
|---|---|---|
| `fast` | `ordered: false, maxRetransmits: 0, priority: high` | pointer moves, batches, enter/leave/over/out, wheel, zoom, mouse deltas |
| `reliable` | `ordered: true, priority: high` | pointer down/up/cancel, keys, text, lifecycle, ping, stats, keyframe requests |
| `bulk` | `ordered: true` | clipboard, drag, drop |

The host's routing decision is `is_fast(opcode)` and is taken from the opcode alone, not from
which channel the message arrived on. A frame carrying a fast opcode goes on the lossy internal
queue wherever it came from.

## Opcodes

| Code | Name | Direction | Channel | Fast |
|---:|---|---|---|---|
| `0x01` | `POINTER_DOWN` | C→H | reliable | |
| `0x02` | `POINTER_UP` | C→H | reliable | |
| `0x03` | `POINTER_MOVE` | C→H | fast | yes |
| `0x04` | `POINTER_CANCEL` | C→H | reliable | |
| `0x05` | `POINTER_ENTER` | C→H | fast | yes |
| `0x06` | `POINTER_LEAVE` | C→H | fast | yes |
| `0x07` | `POINTER_OVER` | C→H | fast | yes |
| `0x08` | `POINTER_OUT` | C→H | fast | yes |
| `0x09` | `POINTER_MOVE_BATCH` | C→H | fast | yes |
| `0x10` | `WHEEL` | C→H | fast | yes |
| `0x11` | `ZOOM` | C→H | fast | yes |
| `0x20` | `KEY` | C→H | reliable | |
| `0x21` | `TEXT_INPUT` | C→H | reliable | |
| `0x22` | `COMPOSITION_UPDATE` | C→H | reliable | |
| `0x30` | `CLIPBOARD` | C→H | bulk | |
| `0x40` | `DRAG` | C→H | bulk | |
| `0x41` | `DROP` | C→H | bulk | |
| `0x50` | `FOCUS_STATE` | C→H | reliable | |
| `0x51` | `VISIBILITY` | C→H | reliable | |
| `0x52` | `RESIZE` | C→H | reliable | |
| `0x53` | `POINTERLOCK_STATE` | C→H | reliable | |
| `0x54` | `MOUSE_DELTA` | C→H | fast | yes |
| `0x60` | `PING` | C→H | reliable | |
| `0x61` | `PONG` | **H→C** | reliable | |
| `0x62` | `STATS` | C→H | reliable | |
| `0x63` | `BYE` | **H→C** | reliable | |
| `0x64` | `KEYFRAME_REQUEST` | C→H | reliable | |

`0x62` used to be defined in both directions — the host→client twin was a 25-byte injector-queue
frame that was never sent. It is gone: the injector's counters go to `SessionTelemetry`, where
`status --json` reads them beside everything else, so the opcode now means one thing.

## The optional client stamp

Five frame types may carry a trailing `u64` of the client's own monotonic nanoseconds — the same
timebase as the `PING` stamp, `performance.now() * 1e6`. It is what lets the host measure how
long an event spent in flight without the client reporting anything back; see `input_recv` in
`docs/TELEMETRY.md`.

**Every tail is appended past the v1 length and read only when the whole of it is present.** An
older client sends no tail and the host reads 0, which is the "not stamped" value. A newer
client may append further fields and this host reads the prefix it knows. The declared length is
a floor, never an equality — the same rule the stats frame follows.

| Frame | v1 length | Stamp at | Sequence at |
|---|---:|---:|---:|
| `POINTER_*` (`0x01`–`0x08`) | 40 | 40 | 48 |
| `POINTER_MOVE_BATCH` (`0x09`) | `10 + n*24` | `10 + n*24` | `20 + n*24` |
| `WHEEL` (`0x10`) | 15 | 15 | 23 |
| `KEY` (`0x20`) | variable | just past the `key` string | — |
| `MOUSE_DELTA` (`0x54`) | 8 | 9 | 17 |

## The optional fast-channel sequence

The four fast frames above may carry a `u16` after the stamp: a per-session counter the client
increments once per frame it sends on the `fast` channel.

That channel is `ordered: false, maxRetransmits: 0`, and the host merges its two internal queues
with a `crossbeam::select!` that picks **randomly** among ready operations, so there are three
independent reorder points between "the user moved, then clicked" and "the host injected". The
consequence that mattered — a click landing where the cursor used to be — is already fixed:
`POINTER_DOWN`/`POINTER_UP` inject the coordinates they carry. What remains is that a *reordered*
move is replayed rather than rejected, walking the cursor backwards and then forwards again for
one frame. The 50 ms age gate bounds how stale a replayed move can be and says nothing about
order, because there was nothing to compare.

The host drops a **droppable** fast event whose sequence is behind the highest it has already
injected, comparing `(seq - last) as i16 > 0` so the wrap at 65535 is correct, and counts it in
`input_reorder_drops`. Only a *newer* sequence advances that high-water mark. Buttons, keys,
clipboard and lifecycle are never gated: a reordered state transition is not recoverable and must
be injected whatever its sequence says. A `POINTER_LEAVE` or `POINTER_OUT` — fast but not
supersedable — advances the counter without being droppable.

**`MOUSE_DELTA` is never dropped for order.** Its position term is a relative *increment* that
both injector backends accumulate into a position they track themselves, and increments commute —
applying a reordered one was always exactly correct, and dropping it loses that motion
permanently, under pointer lock, where the user can least compensate. What a late delta must not
do is restate a stale button word, which `button_transitions` would read as a press or a release,
so the injector replaces that word with the newest one it has already injected before dispatching
it. The age gate is unchanged and still drops a stale delta: 50 ms-old motion is worth less than
the jump it would cause.

The sequence space is per channel. `parse` decides `fast` from the opcode alone and not from the
channel a message arrived on, so a fast opcode sent on `reliable` still lands on the lossy
internal queue; that is unchanged and intentional. Frames on `reliable` carry no sequence.

**An absent tail means the session is simply not reorder-gated**, which is exactly right for a
client older than the field: the host has nothing to compare and injects everything, as before.

The shipped client gates it on capability bit 5 **and** on the stamp, because the host finds the
sequence by offset and it sits past the stamp — sending one without the other would put two bytes
where nothing reads them. Its counter is per page, not per session, and is never reset: the host's
high-water mark is per session and starts at "none", so a counter that only moves forward is right
on both sides of a reconnect, while one that restarted at 0 against a host that had not would be
rejected for half the sequence space. `scripts/test-client-stats.mjs` asserts the four offsets and
that a reliable frame carries no sequence.

## Frame layouts

### `POINTER_*` — `0x01`–`0x08`, 40 bytes (+8)

```
0      u8   opcode
1      u8   source: 0 mouse, 1 touch, 2 pen
2      u32  pointerId
6      f32  x, normalized 0..1 of the client surface
10     f32  y
14     f32  pressure 0..1
18     f32  tiltX degrees
22     f32  tiltY degrees
26     f32  twist degrees
30     f32  contact width, normalized
34     f32  contact height, normalized
38     u16  buttons bitmask
--- optional ---
40     u64  client stamp, ns
```

The coordinates on a `DOWN`/`UP` are authoritative and the host applies them, which is what makes
a touch client's first tap land under the finger — browsers fire `pointerdown` with no
`pointermove` before it. The one exception is pointer lock: while the client holds it the browser
freezes `clientX`/`clientY` at the lock point, so those coordinates are stale by construction and
only `MOUSE_DELTA` moves the cursor. `protocol::positions_cursor` owns that decision for both
backends.

### `POINTER_MOVE_BATCH` — `0x09`, `10 + n*24` bytes (+10)

```
0      u8   opcode
1      u8   source
2      u32  pointerId
6      u16  buttons
8      u16  sample count n (1..=40; a batch of 0 is rejected)
10     n * 24 bytes of sample: f32 x, y, pressure, tiltX, tiltY, twist
--- optional tail ---
N      u64  client stamp of the LAST sample, ns          (N = 10 + n*24)
N+8    u8   reserved, always 0
N+9    u8   predicted count: how many TRAILING samples came from getPredictedEvents()
N+10   u16  fast-channel sequence
```

The predicted count is clamped to `n` rather than trusted. The host drops predicted samples for
pen and touch — a stroke is a shape, and a guess the next real sample corrects still leaves a
kink in the ink — and keeps them for the cursor, which is last-value-wins and gets corrected a
frame later.

Injection: Windows builds the whole cursor run into one `SendInput` array; pen and touch stay one
`InjectSyntheticPointerInput` per sample because that API takes a frame of *simultaneous
contacts* capped at the device's `maxCount`, not a sequence over time. macOS has no batch form of
`CGEventPost`, so the cursor run collapses to its last sample carrying the whole run's delta.

### `WHEEL` — `0x10`, 15 bytes (+8)

```
0      u8   opcode
1      u8   source
2      f32  deltaX
6      f32  deltaY
10     f32  deltaZ
14     u8   deltaMode: 0 pixel, 1 line, 2 page
--- optional ---
15     u64  client stamp, ns
```

### `ZOOM` — `0x11`, 5 bytes

```
0      u8   opcode
1      f32  delta (relative pinch scale change)
```

### `KEY` — `0x20`, variable (+8)

```
0      u8   opcode
1      u8   flags: bit0 down, bit1 repeat
2      u16  modifier bitmask
4      u8   len(code)
5      ..   code, UTF-8 (a W3C KeyboardEvent.code)
+      u8   len(key)
+      ..   key, UTF-8
--- optional ---
+      u64  client stamp, ns
```

### `MOUSE_DELTA` — `0x54`, 8 bytes (+1 flags, +8 stamp)

```
0      u8   opcode
1      u8   source (always 0)
2      i16  dx
4      i16  dy
6      u16  buttons
--- optional ---
8      u8   flags (see below)
9      u64  client stamp, ns
17     u16  fast-channel sequence
```

Byte 8 was already a zero pad every shipped client sent, so it became the flags byte without a
version bump: an old client reads as "no flags set".

### Event flags (`evflag`)

| Bit | Name | Meaning |
|---:|---|---|
| 0 | `UNADJUSTED` | The client holds pointer lock with `unadjustedMovement: true`, so the deltas are raw device counts with no OS pointer curve applied |

Applies to `MOUSE_DELTA` byte 8 only. The `POINTER_MOVE_BATCH` tail has a byte in the same
position and it is reserved: `UNADJUSTED` means the client holds pointer lock with raw
movement, and a locked pointer sends `MOUSE_DELTA` and never a batch, so no bit defined here
can ever be set on one. The client writes 0 and the host does not read it.

**The bit selects which side applies the one pointer curve, and there must be exactly one.**
Without it the client's own OS has already curved the deltas and the host must not curve them
again, so the Windows backend injects them absolutely from a position it tracks itself. With it
the deltas are raw device counts nobody has curved — `settings.sensitivity` defaults to 1.0 and
`settings.accel` to 0 — so the Windows backend injects them as relative `MOUSEEVENTF_MOVE` and
Windows' own pointer speed and "enhance pointer precision" apply, which is the user's configured
curve on the machine being driven. Curving neither way was a two-to-six-times gain reduction that
read as a pointer lagging the hand. `PointerScope::Display` keeps the tracked-absolute path in
both cases, because confining the cursor to one monitor needs a position to clamp and relative
motion has none.

On macOS `CGEventPost` does not apply the system's pointer acceleration to synthetic events, so
there is no host curve to hand the motion back to; that backend tracks the position in both cases.

### `KEYFRAME_REQUEST` — `0x64`, 2 bytes

```
0      u8   opcode
1      u8   reason (0 = unspecified; the only value sent today)
```

The last resort for a receiver that cannot ask for a keyframe any other way. A client running the
legacy `createEncodedStreams` transform (Chrome and Edge 86–140) takes the frames *instead of*
libwebrtc's reference finder, so nothing downstream emits an RTCP PLI and the host would never
learn the picture is broken. The host routes it to the same `request_idr()` a PLI takes,
rate-limited to one per 200 ms because the client's own limiter is advisory.

### `PING` / `PONG` — `0x60` / `0x61`

```
PING   0x60                                     9 bytes,  C→H
0      u8   opcode
1      u64  client stamp, ns

PONG   0x61                                     19 bytes, H→C
0      u8   opcode
1      u64  echoed client stamp, as sent in the PING
9      u64  host_now_ns at the moment the PONG was built
--- optional ---
17     u16  host capabilities (see below)
```

The echoed half gives the client its RTT; the host half is what lets it place a host timestamp on
its own timeline at all. Bytes 0..17 are frozen: a client older than a field reads the prefix it
knows and ignores the rest, and a host older than a field sends a shorter frame.

### `BYE` — `0x63`, 2 bytes, H→C

```
0      u8   opcode
1      u8   reason: 0x01 kicked, 0x02 host exiting
```

### `STATS` — `0x62`, C→H

104 bytes in v3, 96 in v2, 72 in v1 — each tail read whole or not at all, so a host built
against any of the three reads the prefix it knows. Layout, tier bytes, path codes and
degradation flags are in `docs/TELEMETRY.md`.

### The remaining frames

`TEXT_INPUT`/`COMPOSITION_UPDATE` (`0x21`/`0x22`) are `u32` length + UTF-8. `CLIPBOARD` (`0x30`)
is an op byte, a `u32`-length MIME string and a `u32`-length payload. `DRAG` (`0x40`) is a phase
byte and two `f32` coordinates. `DROP` (`0x41`) is two `f32` coordinates, a `u16` item count and
that many variable-length items (`u16` name, `u32` MIME, `u64` size, `u64` data length, data);
the count is bounded at 256 items and against the message length before it sizes anything, since
it otherwise sizes a heap allocation from an unvalidated number. `FOCUS_STATE`, `VISIBILITY` and
`POINTERLOCK_STATE` (`0x50`, `0x51`, `0x53`) are one boolean byte. `RESIZE` (`0x52`) is `u16`
width, `u16` height, `f32` devicePixelRatio.

## Host capabilities

The client must not assume the host understands anything optional. Every `PONG` — which it
already receives once a second — carries a `u16` bitfield at offset 17 saying what this host
actually does. **A frame shorter than 19 bytes means no bits: an older host, and the correct
answer for it.** Read it as:

```js
const caps = dv.byteLength >= 19 ? dv.getUint16(17, true) : 0;
```

| Bit | Name | Set when the host |
|---:|---|---|
| 0 | `KEYFRAME_REQUEST` | parses `0x64` and routes it to the encoder's IDR path |
| 1 | `STATS_V2` | reads the 96-byte v2 stats frame, not only its 72-byte v1 prefix |
| 2 | `UNRELIABLE_MOVE_CHANNEL` | accepts a separate unordered/unreliable channel for pointer moves |
| 3 | `INPUT_TIMESTAMP` | reads the optional trailing client stamp and reports `input_recv` |
| 4 | `INPUT_EVENT_FLAGS` | reads the `MOUSE_DELTA` flags byte, so `unadjustedMovement` is worth reporting |
| 5 | `INPUT_SEQUENCE` | reads the optional fast-channel `u16` sequence and rejects a reordered positional event |

This build sets bits 0, 1, 3, 4 and 5. **Bit 2 is deliberately clear and is expected to stay clear**:
the existing `fast` channel is already `ordered: false, maxRetransmits: 0`, so a second one has
nothing to add. The bit is reserved rather than reused so it cannot come to mean something else.

The bits are a floor, not a contract to stop sending: everything they gate is additive and safely
ignored by a host that does not set them. A client seeing bit 0 clear should keep its `<video>`
demotion watchdog armed rather than waiting for an IDR that is not coming; a client seeing bit 3
clear may skip the eight stamp bytes, but sending them anyway costs the host nothing.

`HOST_CAPABILITIES` in `protocol.rs` is the one place the value is assembled, and
`the_pong_tail_announces_what_this_host_understands` asserts both the frozen prefix and every
bit.

## Host-side queueing

`route()` puts a fast opcode on a 64-deep bounded channel and everything else on an unbounded
one; a single `injector` thread drains both with `crossbeam::select!` and is the only place the
OS injection APIs are touched.

Two things can drop an event, and both are counted:

- **`input_queue_drops`** — the fast queue was full. The *oldest* entry is shed, not the arriving
  one: when the queue is full the events in it are the stale ones and the arriving event is where
  the pointer actually is.
- **`input_stale_drops`** — the event waited more than 50 ms (three 60 Hz frames) and is
  positional **from a mouse**. Keys, buttons, clipboard and lifecycle are state transitions the
  host cannot recover from, so they are never dropped for age however far behind the injector is
  — and neither are pen or touch samples. Only where a *cursor* is now is observable, so a 50 ms
  old position is worse than nothing; a pen or touch sample is not redundant with its neighbours,
  it is the shape of the line, so dropping one leaves a permanent straight segment where the user
  drew a curve and no later event repairs it. Late ink is still the right ink.
- **`input_reorder_drops`** — a supersedable fast event arrived with a sequence behind one
  already injected. See "The optional fast-channel sequence" above.
- **`input_parse_errors`** — the frame did not decode. `parse` returns a typed `ParseError`
  naming the opcode, the length it needed and the reason (unknown opcode, truncated, a length
  prefix past the end, invalid UTF-8), and the transport logs one line per distinct error per
  channel so a client whose cached `input.js` has drifted from the host is diagnosable without a
  flood.

## Adding a field

1. Append it past the current length of the frame; never change an existing offset.
2. Read it only when the whole of it is present, defaulting to 0 — the length is a floor.
3. Add the layout to the table above and a `parse` test for the old length and the new one.
4. If the client must know the host understands it, add a bit to `HOST_CAPABILITIES` and the
   table under "Host capabilities". A field the client can send unconditionally — because a host
   that does not read it is unharmed — needs no bit at all; one that changes what the client
   *does* (whether to arm a fallback, say) needs one.
