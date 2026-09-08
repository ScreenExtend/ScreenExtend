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

| Frame | v1 length | Stamp at |
|---|---:|---:|
| `POINTER_*` (`0x01`–`0x08`) | 40 | 40 |
| `POINTER_MOVE_BATCH` (`0x09`) | `10 + n*24` | `10 + n*24` |
| `WHEEL` (`0x10`) | 15 | 15 |
| `KEY` (`0x20`) | variable | just past the `key` string |
| `MOUSE_DELTA` (`0x54`) | 8 | 9 |

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
N+8    u8   flags (see below)
N+9    u8   predicted count: how many TRAILING samples came from getPredictedEvents()
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
```

Byte 8 was already a zero pad every shipped client sent, so it became the flags byte without a
version bump: an old client reads as "no flags set".

### Event flags (`evflag`)

| Bit | Name | Meaning |
|---:|---|---|
| 0 | `UNADJUSTED` | The client holds pointer lock with `unadjustedMovement: true`, so the deltas are raw device counts with no OS pointer curve applied |

Applies to `MOUSE_DELTA` byte 8 and the `POINTER_MOVE_BATCH` tail flags byte. Without it the
client's own OS has already curved the deltas, and the host must not curve them a second time —
which is why both backends now inject relative motion absolutely from a position they track
themselves, rather than letting the host OS's pointer ballistics compound with the client's.

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
PING   0x60 + u64 client stamp, ns              9 bytes,  C→H
PONG   0x61 + u64 echoed stamp + u64 host_now_ns  17 bytes, H→C
```

The echoed half gives the client its RTT; the host half is what lets it place a host timestamp on
its own timeline at all. A client older than the second field reads the first 9 bytes and ignores
the rest.

### `BYE` — `0x63`, 2 bytes, H→C

```
0      u8   opcode
1      u8   reason: 0x01 kicked, 0x02 host exiting
```

### `STATS` — `0x62`, C→H

96 bytes in v2, 72 in v1. Layout, tier bytes and degradation flags are in `docs/TELEMETRY.md`.

### The remaining frames

`TEXT_INPUT`/`COMPOSITION_UPDATE` (`0x21`/`0x22`) are `u32` length + UTF-8. `CLIPBOARD` (`0x30`)
is an op byte, a `u32`-length MIME string and a `u32`-length payload. `DRAG` (`0x40`) is a phase
byte and two `f32` coordinates. `DROP` (`0x41`) is two `f32` coordinates, a `u16` item count and
that many variable-length items (`u16` name, `u32` MIME, `u64` size, `u64` data length, data);
the count is bounded at 256 items and against the message length before it sizes anything, since
it otherwise sizes a heap allocation from an unvalidated number. `FOCUS_STATE`, `VISIBILITY` and
`POINTERLOCK_STATE` (`0x50`, `0x51`, `0x53`) are one boolean byte. `RESIZE` (`0x52`) is `u16`
width, `u16` height, `f32` devicePixelRatio.

## Host-side queueing

`route()` puts a fast opcode on a 64-deep bounded channel and everything else on an unbounded
one; a single `injector` thread drains both with `crossbeam::select!` and is the only place the
OS injection APIs are touched.

Two things can drop an event, and both are counted:

- **`input_queue_drops`** — the fast queue was full. The *oldest* entry is shed, not the arriving
  one: when the queue is full the events in it are the stale ones and the arriving event is where
  the pointer actually is.
- **`input_stale_drops`** — the event waited more than 50 ms (three 60 Hz frames) and is
  positional. Only pointer moves, batches and mouse deltas can be dropped this way. Keys,
  buttons, clipboard and lifecycle are state transitions the host cannot recover from, so they
  are never dropped for age however far behind the injector is.

## Adding a field

1. Append it past the current length of the frame; never change an existing offset.
2. Read it only when the whole of it is present, defaulting to 0 — the length is a floor.
3. Add the layout to the table above and a `parse` test for the old length and the new one.
4. If the client must know the host understands it, that is what the stats frame's version byte
   is for; a one-directional field needs no negotiation at all.
