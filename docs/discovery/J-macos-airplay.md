# J — macOS AirPlay display fallback (`src-tauri/src/macos_utils/airplay/`)

Territory: all ten files, 4958 lines, read in full.
Platform: macOS only (`pub mod airplay;` at `macos_utils/mod.rs:1`, no further `cfg` gate — it compiles
on every macOS build, and its `plist`/`httparse` deps sit in the macOS-only dependency block,
`src-tauri/Cargo.toml:131-133`).

## What this is, in one paragraph

Below macOS 10.14.4 the private `CGVirtualDisplay*` classes do not exist, so nothing in userspace can
add a display. This module borrows one from AirPlay: it stands up a **fake AirPlay receiver** (Bonjour
`_airplay._tcp` + `_raop._tcp`, an RTSP/HTTP control socket, a static `/fp-setup` table, an
accept-and-discard mirroring socket), then drives the Mac's *own* AirPlay picker at it through the
Accessibility API, so **macOS** creates the display. Once it exists it is an ordinary
`CGDirectDisplayID` and the unchanged CGDisplayStream + VideoToolbox + WebRTC pipeline captures it.
There is no decode, no decrypt, no VideoToolbox anywhere in this territory. The tier is selected by
`virtual_display.rs::probe_display_backend()` → `AirPlayFallback` when `AnyClass::get("CGVirtualDisplay")`
misses (`macos_utils/virtual_display.rs:334-361`), and it caps concurrency at one display
(`mod.rs:390-392`).

---

## 1. Files and purpose

**`mod.rs` (630)** — the `VirtualDisplayController` implementation and the whole session choreography.
Owns a private 2-worker tokio runtime (`mod.rs:92-102`) so `create_display` (called from
`spawn_blocking`) and `remove_all_displays` (called from a bare `std::thread::spawn` at shutdown, with
no runtime context) share one path. Holds the warm-receiver slot, the session registry, the
`Cancel` token type, `diagnose()`, and the ignored `live_end_to_end` harness. `create_display` is the
spine: clamp geometry → ensure the receiver is advertising → press the picker → wait for the display
→ un-mirror it → restore every other display's mode → move it onto the closest published mode.

**`receiver.rs` (685)** — the receiver's session state machine and RTSP verb dispatch. Binds five
sockets (control TCP, mirror TCP, timing UDP, two decoy audio UDP), publishes both Bonjour services,
spawns the control accept loop and the mirror drain task. `handle()` is a flat
`match (method, url)`. Deliberately parses no key material: the doc at `receiver.rs:18-20` is explicit
that `ekey`/`eiv`/`streamConnectionID` are the AES-CTR inputs and none of them is read.

**`rtsp.rs` (329)** — the mixed RTSP/1.0 + HTTP/1.1 control socket. `hyper` cannot parse or emit
`RTSP/1.0`, so this is a raw `TcpStream` plus `httparse`, with a four-byte version-token swap in a
scratch copy of the request line (bounded to the first line — there is a test for exactly that,
`rtsp.rs:305-312`). Size ceilings: 64 KiB head, 1 MiB body, 48 headers, 20 s idle.

**`info.rs` (342)** — every binary plist the receiver emits: the qualifier form of `/info`, the full
device description (where the requested geometry lives, and where audio is declined by advertising
empty `audioFormats`/`audioLatencies`), the two SETUP replies. Also the two SETUP-body parsers.
Carries the one genuinely non-obvious protocol fact: `refreshRate` is a frame *interval* in seconds,
not a rate (`info.rs:8-10`, `info.rs:115-118`).

**`fairplay.rs` (224)** — the static `/fp-setup` tables, transcribed from UxPlay's
`lib/fairplay_playfair.c` (LGPL-2.1-or-later, used via GPL-3 / AGPL-3 §13, attribution at
`fairplay.rs:12-17`). Four canned 142-byte replies selected by `req[14]`, plus a 12-byte header and a
20-byte echo for message 2. No key material.

**`mirror.rs` (296)** — accept, frame, discard. Reads the 128-byte header, takes `header[0..4]` as the
little-endian payload length and `header[4]` as the packet type, drains the payload into a reused
64 KiB scratch buffer. Also holds `TimingSocket` (bound, never read) and `configure_keepalive`
(raw `setsockopt` for `SO_KEEPALIVE` + `TCP_KEEPALIVE`).

**`dnssd.rs` (555)** — hand-written FFI against `dns_sd.h` in libSystem (`DNSServiceRegister`,
`DNSServiceRefDeallocate`, the five `TXTRecord*` functions). Derives a stable synthetic identity from
the machine's `IOPlatformUUID`, picks a real interface index (never 0 — the reason is the best comment
in the module, `dnssd.rs:58-69`), and publishes both services with RAII withdrawal.

**`sender_prefs.rs` (286)** — the module that decided *not* to do its job, and says so at length
(`sender_prefs.rs:1-21`). It measured that `com.apple.coremedia`'s
`wirelessdisplaymac_set_display_size` is a dead end (CoreDisplay's custom-geometry branch also needs a
`…virtualDisplayResolution` key nothing writes), so it writes nothing. What survives is the geometry
ceiling (1920x1080, 60 Hz), the advertised constant, `clamp()`, and a read-only probe. The write
machinery is still present and unreachable.

**`topology.rs` (575)** — finding the created display and taking it out of the mirror set. Pure public
CoreGraphics. Identifies the display by `CGDisplayVendorNumber`/`ModelNumber` = `b"aapl"`/`b"airp"`
rather than by diffing the active list, because macOS reuses display ids across sessions. Uses the
**online** list, not the active one, because a mirror slave is excluded from
`CGGetActiveDisplayList` and would look like a failed session. Polls rather than registering a
reconfiguration callback, because those are delivered on the main run loop and `ScreenExtend serve`
never starts one (`topology.rs:5-10`) — a genuinely important observation that applies beyond this
module.

**`ax.rs` (1036)** — the Accessibility driver for the Displays menu extra. The public route
(`AVOutputContext`) needs `com.apple.avfoundation.allow-system-wide-context`, which AMFI will not
honour for a third-party binary, so this presses the menu instead. Matches rows by `AXIdentifier`
(the action selector, locale-independent) where available and by strings read out of
`Displays.menu`'s own bundle where not — because `AXIdentifier` on menu items is 10.15-only and this
tier exists for 10.13/10.14.

---

## 2. Function catalog

### `mod.rs`
- `Cancel::{new, cancel, is_cancelled, cancelled}` — 40-line cancellation token (`tokio-util` is not a
  dependency). `cancelled()` loops on `Notify::notified()` re-checking the flag, so it is not
  lost-wakeup-prone.
- `runtime()` — `OnceLock<Runtime>`, 2 workers, `thread_name("se-airplay")`. `.expect()`s on build.
- `warm_receiver()` — `OnceLock<Mutex<Option<Receiver>>>`; the receiver kept alive across sessions.
- `receiver_name()` — returns the constant `"ScreenExtend"`. Allocates a `String` each call.
- `ensure_receiver()` — locks the warm slot, and if empty `runtime().block_on(Receiver::start(...))`
  **while holding the guard**. Returns whether it had to cold-start.
- `prewarm()` — spawns `se-airplay-warm` to call `ensure_receiver()` at backend-selection time; the
  spawn result is `.ok()`'d away.
- `registry()` — `OnceLock<Mutex<SessionRegistry>>`; at most one entry.
- `owns_display(id)` — the only symbol other modules use besides the controller
  (`virtual_display.rs:394, 400`).
- `availability()` — checks `ax::accessibility_trusted()` only, though its doc names two requirements.
- `AirPlayVirtualDisplay::new_shared()` — logs unavailability, clears the registry, `prewarm()`s, and
  returns `Some` regardless.
- `create_display` — the spine; see §11.
- `display_device_name(id)` — re-resolves by identity because un-mirroring can renumber displays;
  rewrites the registry key when it changed. Falls back to `display::display_by_name`.
- `max_concurrent_displays()` — `Some(1)`. The only non-`None` implementation in the codebase.
- `remove_display(id)` — restore granted mode → `release_display_configuration()` →
  `ax::disconnect()` → restore other displays → `wait_for_removal` twice.
- `remove_all_displays()` — loop `remove_display`, then drop the receiver, then `sender_prefs::restore()`.
- `diagnose(&Receiver)` — turns a `Phase` into a user-facing sentence.
- `probe()` — reports the picker contents. **No caller.**

### `receiver.rs`
- `Receiver::start(name, geometry)` — binds five sockets, publishes Bonjour, spawns two tasks.
- `Receiver::{phase, peer, requests, mirror_connected, geometry_changed, idle_for, stop}` — accessors;
  `geometry_changed` and `idle_for` have no callers.
- `Drop for Receiver` — `stop()` + a summary log.
- `serve_control(listener, inner, cancel)` — accept loop; records the peer, spawns a per-connection
  task racing `serve_connection` against `cancel`.
- `serve_connection(stream, inner)` — read/handle/write loop with a 20 s idle timeout; reuses one
  8 KiB `Vec` across requests.
- `handle(req, inner)` — the verb table. HTTP/1.1 requests get a flat 404 (AirPlay video/HLS, which we
  do not offer); unknown RTSP verbs get 200 rather than 501, deliberately.
- `handle_info` — advances `Advertised → Probed`; returns the TXT blob for the qualifier form,
  otherwise the full description.
- `wants_txt_airplay(body)` — parses the plist looking for `qualifier: ["txtAirPlay"]`.
- `handle_fp_setup` — delegates to `fairplay::respond`, logging `fairplay::describe` on error.
- `handle_setup` — timing phase (detected by `ekey`/`eiv` presence) vs streams phase; answers unknown
  stream types with a bound-but-drained port rather than refusing, because refusal is a disconnect
  condition for the reference receivers.
- `hex_bytes(s)` — hex → bytes, non-hex silently becomes 0.

### `rtsp.rs`
- `Proto::token()` — `"RTSP/1.0"` / `"HTTP/1.1"`.
- `Request::{header, content_type, is_binary_plist}` — `header` lowercases the *lookup* key each call.
- `Response::{ok, status, header, body, closing, encode}` — `encode` builds the whole response with
  seven `format!` allocations plus the body copy.
- `read_request(stream, buf)` — see §6 and the quadratic-scan finding.
- `write_response(stream, proto, cseq, response)` — one `write_all` + `flush`.
- `find_head_end(buf)` — `buf.windows(4).position(...)`, from index 0, every time.
- `normalize_version(head)` — swaps `RTSP/1.0` → `HTTP/1.1` in the request line only.

### `info.rs`
- `Geometry::new(w, h, hz)` — floors w/h at 2, maps `hz == 0` to 60, sets `max_fps = DISCARD_FPS = 1`.
- `data`, `hex_to_bytes`, `encode` — helpers. `encode` logs and returns an **empty `Vec`** on failure.
- `txt_only(blob)`, `full(name, id, geom)`, `setup_timing(port)`, `setup_streams(entries)` — the four
  emitted plists.
- `requested_stream_types(body)` / `is_timing_setup(body)` — the two parsers; both return
  empty/false on any parse failure.

### `fairplay.rs`
- `respond(req)` — length-checked dispatch; version byte must be 3; table index bounds-checked.
- `describe(req)` — a one-line classification for the error log.

### `mirror.rs`
- `MirrorSink::{bind, port, run}` — `run` accepts **one** connection and returns (see the blocker).
- `drain(stream, stats)` — the framing loop.
- `configure_keepalive(stream)` — two `setsockopt`s, both return values ignored.
- `TimingSocket::bind()` — binds a non-blocking UDP socket that is never read.

### `dnssd.rs`
- `primary_interface_index()` — first `AF_INET`, `UP|RUNNING`, non-loopback, non-`awdl`/`llw`/`utun`
  interface, in `getifaddrs` order.
- `TxtRecord::{build, set, bytes}` + `Drop` — RAII over `TXTRecordRef`.
- `Service::register(...)` + `Drop` — RAII over `DNSServiceRef`. `unsafe impl Send`.
- `Identity::{derive, device_id_compact}` — stable synthetic identity from `machine_seed()`; the
  lower-case/upper-case asymmetry between the two services is deliberate and tested.
- `machine_seed()` — **forks `/usr/sbin/ioreg`**; falls back to the hostname.
- `fnv1a(s)` — 64-bit FNV-1a.
- `features_word(bool)` / `features_hex(bool)` — `0x527FFEE6`, bit 27 parameterised.
- `airplay_txt_pairs` / `airplay_txt_blob` — the eight-key TXT set and its wire form, shared with the
  `/info` qualifier reply so they cannot drift.
- `Advertisement::publish(...)` — both registrations, withdrawn together on drop.

### `sender_prefs.rs`
- `read()` / `write(Option<i64>)` / `saved()` — the CFPreferences round trip. `write` is unreachable.
- `exact_geometry_enabled()`, `enable_exact_geometry()`, `restore()` — the abandoned lever.
  `restore()` is called but is an unconditional no-op.
- `clamp(w, h, hz)` — the live one. Clamps to 1920x1080, forces 60 Hz, returns a human note.

### `topology.rs`
- `baseline()` / `Baseline::newcomers()` — active-list snapshot. `newcomers()` has no caller.
- `online_displays()` — `CGGetOnlineDisplayList` into a fixed `[u32; 16]`.
- `is_airplay_display(id)` — vendor/model == `aapl`/`airp`.
- `current_mode_id(id)` / `snapshot_modes()` / `ModeSnapshot::restore_except(skip)` — capture and
  restore every display's `io_display_mode_id`, batched into one `CGBeginDisplayConfiguration`.
- `find_airplay_display()` — identity search over the online list, excluding the built-in.
- `wait_for_airplay_display(base, timeout)` — 50 ms poll; prefers a newcomer, accepts a pre-existing
  AirPlay display as a fallback (because macOS reuses ids across sessions).
- `wait_for_removal(id, timeout)` — 50 ms poll on the active list.
- `is_mirroring(id)` / `stop_mirroring(id)` — the programmatic "Use As Separate Display".
- `available_modes(id)` — the full published ladder, sorted and deduped.
- `best_effort_mode(id, want)` — exact match, else minimise `2·|ln(aspect ratio error)| + |ln(area
  ratio error)|`. A sensible metric; note it never rejects a bad match, so a 1920x1080-only ladder
  will "best-effort" a 1080x1920 portrait request into landscape.
- `set_mode_permanently(id, w, h)` — the configuration-based mode set, deliberately *not*
  `CGDisplaySetDisplayMode` (see the finding).
- `release_display_configuration()` — `CGRestorePermanentDisplayConfiguration`.
- `identity(id)`, `is_online(id)`, `geometry(id)` — thin CG wrappers. `is_online` has no caller.

### `ax.rs`
- `menu_bar_option_enabled()` / `enable_menu_bar_option()` — read/write
  `com.apple.airplay/showInMenuBarIfPresent`. The write is never undone.
- `menu_string(key)` — `CFBundleCopyLocalizedString` out of `Displays.menu`'s own bundle; filters out
  the "key returned unchanged" miss case. This is what makes title matching locale-safe.
- `menu_strings()` — `OnceLock` cache of `AIRPLAY_TURN_OFF`, `USE_AS_DISPLAY`, and the three
  `AX_AIRPLAY_*` descriptions.
- `CfStr` / `CfValue` / `OwnedElement` + `Drop` — CF RAII wrappers.
- `cfstring_to_string(s)` — type-checked, 1024-byte stack-ish buffer, `false` on overflow.
- `copy_attr` / `attr_string` / `is_enabled` / `children` — AX attribute readers. `children` leaks
  (see the finding); `is_enabled` treats a missing `AXEnabled` as enabled.
- `press` / `perform` — `AXUIElementPerformAction` with a five-case error mapping.
- `accessibility_trusted()` — `AXIsProcessTrusted()`.
- `system_ui_server_pid()` — **forks `/usr/bin/pgrep -x SystemUIServer`**.
- `MenuSnapshot::{device_names, offers_extend}` — `offers_extend` has no caller.
- `find_displays_extra()` / `find_displays_extra_once()` — locate the extra, adding it via
  `enable_menu_bar_option()` + `/usr/bin/open` on the bundle path if missing.
- `read_menu(extra)` / `find_row(extra, ident, title)` — the flat row walk. `find_row` requires the row
  to be both matching *and* `is_enabled`, so disabled headers ("AirPlay To:") cannot be pressed.
- `open_menu(extra)` — press-and-poll, up to 3 rounds; the comment explaining why a "reset" press is
  forbidden (`ax.rs:571-578`) is load-bearing.
- `close_menu(extra)` / `press_escape()` — `AXCancel` on the menu child, else a **global** synthetic
  Escape.
- `press_row_in_menu(timeout, find, missing)` — the retry harness: find and press adjacently so the
  element cannot go stale in between.
- `snapshot()` / `connect_to(name)` / `use_as_separate_display()` / `disconnect()` — the four public
  operations.
- `find_stop_row_by_position(extra)` — the 10.15+ positional fallback for the Stop row: the first
  `connectToAirPlayDevice:` item, returned only once a `startMirroring:`/`stopMirroring:` row proves a
  session is live.

---

## 3. Threads

| Thread / task | Spawned at | Priority / QoS | Lifetime | Owns |
|---|---|---|---|---|
| `se-airplay` runtime workers (2) | `mod.rs:92-102`, lazily on first `ensure_receiver`/`remove_all_displays` | default tokio, **no QoS class set** | process lifetime (`OnceLock`, never shut down) | the receiver's async work |
| `se-airplay-warm` | `mod.rs:177-186`, from `new_shared()` | default `std::thread` | one `ensure_receiver()` call | nothing after it returns |
| `serve_control` task | `receiver.rs:138`, on the private runtime | inherits worker | until `Cancel` or accept error | the control `TcpListener` |
| per-connection task | `receiver.rs:229-242` | inherits worker | one control connection | one `TcpStream`, an 8 KiB `Vec` |
| `MirrorSink::run` task | `receiver.rs:142` | inherits worker | **one** mirror connection, then ends | the mirror `TcpListener` (dropped when it ends — the blocker) |
| the calling thread | `spawn_blocking` pool (create) or a bare `std::thread` (shutdown) | tokio blocking pool | one `create_display` / `remove_all_displays` | all the CoreGraphics + AX work, all the `std::thread::sleep`s |

Notes:
- Nothing here is a real-time thread and nothing needs to be — no frame or audio data crosses this
  module.
- The private runtime is deliberate and correct: `remove_all_displays` runs from a thread with no
  runtime context (`mod.rs:86-91`), so borrowing the streamer's would panic.
- `platform::tune_transport_thread` is *not* applied to these workers — the streamer applies it via
  `on_thread_start` (CLAUDE.md, `streamer/mod.rs`), but this runtime is built independently. That is
  fine (no latency-sensitive work) but worth knowing.
- The private runtime is never shut down. Two idle worker threads persist for the process lifetime on
  every macOS build, including 10.15+ where this tier is never selected — because `runtime()` is only
  touched by `ensure_receiver`, which is only reached from `prewarm`/`create_display`, which only run
  on the AirPlay tier. Verified: no other caller.

---

## 4. Locks

| Lock | Guards | Taken by | Hot path? | Across `.await`? |
|---|---|---|---|---|
| `warm_receiver(): Mutex<Option<Receiver>>` | the shared receiver | `ensure_receiver` (`mod.rs:156`), `create_display`'s diagnose path (`mod.rs:296-300`), `remove_all_displays` (`mod.rs:456`) | no | **No `.await`, but `ensure_receiver` holds the guard across `runtime().block_on(Receiver::start(...))`** (`mod.rs:156-168`) — a full socket-bind + Bonjour publish + `ioreg` fork, plausibly hundreds of ms. Not a compiler error (sync fn), but it serialises `prewarm` against the first join. |
| `registry(): Mutex<SessionRegistry>` | the at-most-one session | `owns_display`, `new_shared`, `create_display` (twice), `display_device_name`, `remove_display`, `remove_all_displays`, `Debug` | no | no |
| `Inner::state: Mutex<State>` | phase / last_contact / peer | `serve_control`, `serve_connection`, `handle*`, the five accessors | per request (~1/2 s steady state) | **No.** Every take is either a single statement (`receiver.rs:162`, `:183`, `:191`, `:262`, `:309`, `:342`) or a scoped block with no await inside (`receiver.rs:221-225`, `:235-238`, `:362-367`). Checked specifically for the `if let`/`match` scrutinee temporary-lifetime pattern: **none present in this territory.** The nearest thing is `mod.rs:376-380` (`if let Ok(mut reg) = registry().lock()`), which holds the guard for the whole block — but the block is pure sync map surgery with no await. |
| `sender_prefs::saved(): Mutex<Option<Previous>>` | the abandoned preference's old value | `enable_exact_geometry` (dead), `restore` | no | no |
| `menu_strings(): OnceLock<MenuStrings>` | cached localized titles | first `use_as_separate_display`/`disconnect` | no | n/a — `OnceLock`, but its initialiser does three `CFBundleCreate` round trips |
| `Cancel`: `Arc<AtomicBool>` + `Arc<Notify>` | cancellation | receiver tasks | no | n/a |
| `MirrorStats`: 2×`AtomicU64`, 2×`AtomicBool`, all `Relaxed` | packet/byte counters, latched flags | `drain()` per packet, accessors | per mirror packet | n/a. `Relaxed` is fine for counters; for `connected`/`geometry_changed` it is a latch read from another thread with no ordering requirement, so also fine. |
| `Inner::{requests: AtomicU64, saw_teardown: AtomicBool}` | diagnostics | per request | no | n/a |

**Poisoning is the real risk here, not contention.** `registry()` and `warm_receiver()` are unwrapped
in the create path (`mod.rs:229, 248, 351, 395`) and *silently defaulted* in the teardown path
(`mod.rs:198-199, 449-451, 456`). So a panic anywhere under `create_display` poisons the registry,
after which `remove_all_displays` quietly finds no sessions (`unwrap_or_default()`) and
`warm_receiver().lock()` fails its `if let Ok`, meaning the Bonjour advertisement is never withdrawn
and the AirPlay session is never ended. The failure mode is "the display stays attached forever with
no error", which is the worst possible one for this module.

---

## 5. Buffers and copies

No frame or audio buffer crosses this module. What exists:

| Buffer | Where | Classification |
|---|---|---|
| control read buffer, `Vec::with_capacity(8192)` | `receiver.rs:247` | **reused per connection**, grown as needed. Good. |
| 4 KiB / 8 KiB stack read chunks | `rtsp.rs:155`, `rtsp.rs:215` | stack, no allocation |
| head scratch copy `buf[..head_len].to_vec()` | `rtsp.rs:171` | **CPU memcpy + allocation per request**, purely to swap four bytes |
| body copy `buf[head_len..][..len].to_vec()` | `rtsp.rs:226` | **CPU memcpy + allocation per request** |
| `buf.drain(..consumed)` | `rtsp.rs:227` | **memmove per request** of any pipelined remainder |
| header map | `rtsp.rs:193-202` | **one `String` allocation per header name + one per value, per request**; two keys are ever read |
| response encode | `rtsp.rs:112-133` | `Vec::with_capacity(256 + body)` then ~7 `format!` allocations appended |
| plist encodes | `info.rs:171-178` | fresh `Vec` per response; each `Dictionary` insert allocates a `String` key |
| mirror header `[0u8; 128]` | `mirror.rs:110` | **stack, reused across the whole connection**. Correct. |
| mirror payload scratch `vec![0u8; 65536]` | `mirror.rs:111` | **allocated once per connection, reused per packet**, read in ≤64 KiB slices. Correct — this is the only volume path and it is allocation-free per packet. |
| TXT record buffer | `dnssd.rs:126` | mDNSResponder-owned (dynamic; `TXTRecordCreate(_, 0, null)`), freed on drop |
| `[u32; 16]` display list | `topology.rs:76` | stack; `ids[..count]` then `.to_vec()` — one small allocation per poll iteration, i.e. **once per 50 ms during the attach wait** |
| `Vec<AXUIElementRef>` per `children()` | `ax.rs:304` | one allocation per call, plus a **leaked CFArray** — see the finding |
| `attr_string` | `ax.rs:246-266` | `vec![0i8; 1024]` + a `String` per attribute read, i.e. **two allocations per menu row per poll iteration** |

Ownership transfers: `MirrorSink` moves into its task (`receiver.rs:142`); `Advertisement` moves into
`Receiver`; `Receiver` moves into the warm slot; `ModeSnapshot` moves into `Session` and back out at
`remove_display` (`mod.rs:425`). All by value, no `Arc` churn except `Inner`/`MirrorStats`.

---

## 6. Queues

There are none. No channel, no ring, no bounded queue anywhere in this territory — the only
producer/consumer relationships are TCP sockets, and back-pressure is the kernel's socket buffers.

The two places that matter:
- **The mirror socket is the back-pressure mechanism, by design.** `mirror.rs:9-13`: "Not reading
  leaves the socket's receive buffer full, which back-pressures the sender's encoder and eventually
  produces a reset rather than a stable session." The drain loop exists solely to prevent that.
  Because we advertise `maxFPS = 1` (`info.rs:39-41`, `info.rs:119`) the volume should be trivial —
  **unverified**; nobody has measured whether macOS honours `maxFPS`.
- **The control socket** has a 20 s idle timeout (`rtsp.rs:281`) against a sender that polls
  `POST /feedback` every ~2 s. One outstanding request at a time; the read loop does handle
  pipelining correctly (it checks `find_head_end` before reading).

The `Notify` inside `Cancel` is a broadcast wakeup, not a queue.

---

## 7. Syscall / FFI boundaries

**Process spawns** (each a fork + exec + wait):
- `/usr/sbin/ioreg -rd1 -c IOPlatformExpertDevice` — `dnssd.rs:305`, once per `Identity::derive()`,
  i.e. once per `Receiver::start`.
- `/usr/bin/pgrep -x SystemUIServer` — `ax.rs:367`, once per `find_displays_extra_once()`, which is
  called in a 25 ms retry loop. See the fork-storm finding.
- `/usr/bin/open <Displays.menu>` — `ax.rs:445-448`, blocking `.status()` with no timeout.

**libSystem / `dns_sd.h`** (`dnssd.rs:22-50`) — `DNSServiceRegister`, `DNSServiceRefDeallocate`,
`TXTRecordCreate/Deallocate/SetValue/GetLength/GetBytesPtr`. Symbols are 10.5-era, so no dyld
exposure risk. `TXTRecordRefRaw` is `#[repr(C, align(8))] [u8; 16]`, matching the header's
`union { char PrivateData[16]; char *ForceNaturalAlignment; }` and its `sizeof == 16` assertion —
correctly modelled. `port.to_be()` is correct (the API wants network byte order). The callback is
`NULL`, which is what avoids an event loop — and also what makes an auto-rename invisible.

**libc** — `getifaddrs`/`freeifaddrs`/`if_nametoindex` (`dnssd.rs:74-113`), `setsockopt` ×2
(`mirror.rs:161-178`, both return values discarded), `dlsym` is *not* used here.

**CoreFoundation** (`sender_prefs.rs:47-62`, `ax.rs:53-107`) — `CFPreferencesCopyAppValue`,
`CFPreferencesSetAppValue`, `CFPreferencesAppSynchronize`, `CFRelease`, `CFGetTypeID`,
`CFNumberGetTypeID`, `CFStringGetTypeID`, `CFArrayGetTypeID`, `CFStringCreateWithCString`,
`CFStringGetCString`, `CFArrayGetCount`, `CFArrayGetValueAtIndex`, `CFURLCreateWithFileSystemPath`,
`CFBundleCreate`, `CFBundleCopyLocalizedString`, `CFBooleanGetValue`, `CFBooleanGetTypeID`,
`kCFBooleanTrue`. All 10.0-era. Note the explicit comment at `ax.rs:88-90` that `CFBooleanGetValue`'s
signature must match the other declaration in `macos_utils/streamer/encoder.rs` or the linker clashes —
a real hazard of hand-declared FFI spread across modules, and an argument for one shared `cf_sys`
module.

**ApplicationServices / AX** (`ax.rs:41-51`) — `AXIsProcessTrusted`,
`AXUIElementCreateApplication`, `AXUIElementCopyAttributeValue`, `AXUIElementPerformAction`. All
10.2-era.

**CoreGraphics** — via `objc2-core-graphics` in `topology.rs:22-30` (`CGBeginDisplayConfiguration`,
`CGCompleteDisplayConfiguration`, `CGConfigureDisplayMirrorOfDisplay`,
`CGConfigureDisplayWithDisplayMode`, `CGDisplayCopyAllDisplayModes`, `CGDisplayCopyDisplayMode`,
`CGDisplayIsBuiltin`, `CGDisplayIsInMirrorSet`, `CGDisplayIsOnline`, `CGDisplayMirrorsDisplay`,
`CGDisplayMode::{width,height,refresh_rate,io_display_mode_id}`, `CGDisplayModelNumber`,
`CGDisplayPixelsWide/High`, `CGDisplayVendorNumber`, `CGGetOnlineDisplayList`,
`CGRestorePermanentDisplayConfiguration`), and hand-declared in `ax.rs:93-99`
(`CGEventSourceCreate`, `CGEventCreateKeyboardEvent`, `CGEventPost`).

**Dyld safety: clean.** Everything linked here predates 10.15 comfortably. There are no
`AnyClass::get`/`dlsym`/`msg_send!` runtime lookups in this territory — none are needed, which is
itself notable given the rest of `macos_utils`. `CGDisplayMode::io_display_mode_id` is the only
API worth a second look (it is `CGDisplayModeGetIODisplayModeID`, available since 10.6). `nm -u`
should show nothing new from this module.

**One raw-pointer lifetime pattern worth flagging:** `topology.rs` iterates
`CGDisplayCopyAllDisplayModes` results as `modes.value_at_index(i) as *const CGDisplayMode` and takes
`&*ptr` (`topology.rs:156-160`, `:341-345`, `:441-445`). The array is a `CFRetained` local that
outlives the loop in each case, so the borrows are sound — but it is unchecked `unsafe` deref of a
`CFArray` element with no type check (`CFGetTypeID` is not consulted). Fine as long as the API
contract holds.

---

## 8. Panics

No `unwrap`/`expect` in non-test code except mutex locks and the runtime build. Enumerated:

- `mod.rs:100` `.expect("failed to build the AirPlay receiver runtime")` — reachable if the process is
  out of thread handles or fds. Kills the calling thread; from `spawn_blocking` that becomes a
  `JoinError` at `server.rs:944`, so the join fails rather than the app.
- `mod.rs:156, 229, 248, 351, 395` — `.lock().unwrap()` on `warm_receiver`/`registry`. **Poison
  propagation**: any panic under `create_display` while holding either makes every later
  `create_display` panic too, and silently disables teardown (see §4).
- `receiver.rs:162, 166, 183, 191, 222, 235, 262, 309, 342, 363` — `.lock().unwrap()` on
  `Inner::state`. Poisoning here would make every subsequent request panic, killing the connection
  task; the accept loop survives.
- `sender_prefs.rs:150, 180` — `.lock().unwrap()` on `saved()`. Unreachable in practice
  (`enable_exact_geometry` is dead).
- `topology.rs:83` — `ids[..count as usize]`, where `count` comes from `CGGetOnlineDisplayList`.
  Bounded by the `MAX_DISPLAYS` we pass in, so it cannot exceed 16 unless the API violates its
  contract. Listed for completeness.
- Index expressions that are *not* panics because they are guarded: `fairplay.rs:139`
  (`req[144..164]` under `req.len() == 164`), `rtsp.rs:171/226/270-271`, `mirror.rs:120/127`.
- Test-only: `ax.rs:906, 913, 937, 943` `.expect("fn exists")` / `.expect("fn body")` — these tests
  parse `include_str!("ax.rs")` looking for literal function bodies, so any rename or reformat of
  `press_row_in_menu`/`close_menu` turns a green test red for the wrong reason. Fragile but the intent
  (pinning two mistakes that are easy to reintroduce) is sound.

---

## 9. Silent fallbacks and swallowed errors

This is the module's weakest area — the failure mode of a fake AirPlay receiver is "macOS quietly does
not create a display", and a lot of the evidence is being dropped on the way.

**Errors discarded outright:**
- `mod.rs:186` `.ok()` on the `prewarm` thread spawn — a failed spawn means no pre-warm and a 6 s
  penalty on the first join, unlogged.
- `mod.rs:301, 315, 418` `let _ = ax::disconnect()` / `let _ = ...` — teardown failures ignored on two
  of three paths (the third does log).
- `mirror.rs:163-177` — both `setsockopt` return values discarded. If `TCP_KEEPALIVE` fails, a
  vanished sender leaves us blocked in `read` indefinitely, which is exactly what the keepalive is
  there to prevent.
- `topology.rs:299, 462` `let _ = CGCompleteDisplayConfiguration(config, ForAppOnly)` — the cancel
  path. Reasonable, but a failure here leaks a display configuration.

**Silent degradations (no log):**
- `mod.rs:198-199, 449-451` — a poisoned `registry()` reports "no sessions" via `unwrap_or(false)` /
  `unwrap_or_default()`. Teardown becomes a no-op with no message.
- `mod.rs:456` `if let Ok(mut slot) = warm_receiver().lock()` — a poisoned lock means the receiver is
  never dropped and Bonjour is never withdrawn. No `else`.
- `info.rs:171-178` — a plist encode failure logs but **returns an empty `Vec`**, so the sender gets a
  `200 OK` with a zero-length body and no `Content-Type` mismatch to notice. A malformed `/info`
  answered as a successful empty one is indistinguishable from a working receiver from `mod.rs`'s
  point of view.
- `info.rs:184-197, 202-208` — any plist parse failure yields an empty stream list / `false`.
  `handle_setup` then answers a streams SETUP with the *timing* reply (`receiver.rs:421-429`), which
  is a plausible-looking wrong answer.
- `receiver.rs:384` — same in `wants_txt_airplay`: unparseable body → full description instead of the
  TXT blob.
- `info.rs:59-62` and `receiver.rs:471-474` — `to_digit(16).unwrap_or(0)`: a non-hex character in the
  public key silently becomes `0`. The key is self-generated so this cannot fire, but the pattern
  hides corruption rather than reporting it.
- `dnssd.rs:307-321` — if `ioreg` fails or the UUID line is missing, the seed silently becomes the
  hostname. That changes `deviceid`, `pk` and `pi`, invalidating the sender's pairing cache — a
  behaviour change with no log at all.
- `dnssd.rs:384-386` — a TXT entry longer than 255 bytes is **silently truncated** in
  `airplay_txt_blob` (while `TxtRecord::set` correctly *errors* on the same condition,
  `dnssd.rs:136-138`). The two paths that must not drift are inconsistent about the one case where
  they would.
- `ax.rs:122-124, 141-143` — `CfStr::new` failure → `return false`, indistinguishable from "the
  preference is off".
- `ax.rs:187` `.filter(|v| v != key)` — a bundle-key miss becomes `None`, which then silently disables
  title matching for that row and falls through to the identifier-only path (10.15+ only). On
  10.13/10.14 that means `disconnect()` cannot find the Stop row at all, and the only symptom is the
  generic "not showing an active AirPlay session" message.
- `ax.rs:289-292` — a missing `AXEnabled` is treated as enabled. Deliberate and documented, but it
  means a disabled row on an OS that omits the attribute becomes pressable.
- `ax.rs:296-302` — a non-array `AXChildren` yields an empty vec, which `open_menu` reads as "menu not
  populated" and retries three times before giving up.
- `ax.rs:445-450` — `open` failure is logged but the code proceeds into the 5 s retry loop anyway.
- `topology.rs:79-83` — `CGGetOnlineDisplayList` failure logs and returns empty; every caller then
  behaves as if no displays exist, so `wait_for_airplay_display` burns its full budget and reports
  "macOS did not attach a display within the timeout" rather than "CoreGraphics is failing".
- `topology.rs:151-152` — a display whose mode list is nil is skipped during restore with no message
  (the *found == false* case does log, this one does not).
- `topology.rs:419-422` — a failed mode change returns the current geometry, logged. The caller
  (`mod.rs:333-339`) then logs a *second*, differently-worded message about negotiation. Two logs, one
  event.
- `server.rs:983-986` (adjacent territory) — the `set_display_mode` failure that is near-certain for
  DPR>1 clients on this tier is a `teprintln!` and a `mode_applied = false` that only affects a log
  line. See the finding.

---

## 10. TODO / FIXME / HACK / XXX

**None.** `grep -n "TODO\|FIXME\|HACK\|XXX" *.rs` over the whole territory returns nothing.

---

## 11. Latency path

**This module is not on the glass-to-glass path.** Once `create_display` returns, the result is an
ordinary `CGDirectDisplayID`; capture, encode and transport are territories elsewhere. Nothing here
touches a frame, a sample, or an input event. `input/` is unrelated.

It *is* the entire **join** path for this tier, and it is slow. Per-stage estimate, warm receiver
(cold adds 6000 ms flat):

| Stage | Where | Estimated cost | Serialisation |
|---|---|---|---|
| `availability()` + registry check | `mod.rs:246-254` | µs | — |
| `sender_prefs::clamp` | `mod.rs:256` | µs | — |
| `ensure_receiver()` (warm) | `mod.rs:271` | µs (lock + `is_some`) | takes `warm_receiver` |
| `ensure_receiver()` (cold) | `mod.rs:271-274` | socket binds + `ioreg` fork + 2× `DNSServiceRegister` + **6000 ms fixed sleep** | holds `warm_receiver` across `block_on` |
| `topology::baseline()` + `snapshot_modes()` | `mod.rs:280-284` | ~1 ms (1 CG call + 1 per display) | — |
| `ax::connect_to` | `mod.rs:288` | **typically 0.5–3 s; worst case ~45 s** (3 attempts × (5 s extra-appear + 3×4 s populate + 6 s device row)) | drives real UI; forks `pgrep` per attempt |
| `wait_for_airplay_display` | `mod.rs:290-292` | the AirPlay handshake + WindowServer bring-up; **seconds**, polled at 50 ms | budget = `SESSION_BUDGET` 40 s *minus only what steps 1–5 took* — `ax::connect_to` is outside its own budget |
| **Reconfig A** `stop_mirroring` | `mod.rs:311-321` | 1 CG commit + up to 3 s settle poll; a screen blank | WindowServer-global |
| **Reconfig B** `restore_except` | `mod.rs:327` | 1 CG commit; a second screen blank | WindowServer-global |
| **Reconfig C** `best_effort_mode` | `mod.rs:333` | 1 CG commit + up to 2 s settle poll; a third blank | WindowServer-global |
| **Reconfig D** `pipeline::set_display_mode` | `server.rs:975-977` | `CGDisplaySetDisplayMode` + 1.5 s settle, **or a guaranteed failure** if the requested (DPR-multiplied) size is off the ladder | WindowServer-global |
| `wait_for_display_settle` + `apply_display_scale` | `server.rs:994-995` | more polling | — |

**Every one of those stages is under `DISPLAY_CORRELATION_LOCK`** (`server.rs:928`), held across the
whole block. On this tier that lock is redundant with `max_concurrent_displays() == Some(1)`, but it
does mean a second client's join request blocks for the full duration rather than being rejected
promptly by `display_slot_unavailable` — which runs *before* the lock (`server.rs:127-141`), so
actually it is rejected first. Confirmed no deadlock.

**Teardown** is comparably slow and runs at app quit on a bare thread: `ax::disconnect()` (up to ~50 s
worst case) + two 8 s `wait_for_removal` windows. See
`findings/minor-airplay-teardown-drives-the-menu-bar-during-app-shutdown.md`.

**Steady-state cost while a session is live:** the mirror drain task wakes on each packet. At the
advertised `maxFPS = 1` that is ~1 packet/s plus heartbeats — negligible. If macOS ignores `maxFPS`,
it is a full-rate H.264 stream of the whole desktop being encoded by the host and thrown away by us,
competing for the same GPU as our own VideoToolbox encode. **Unmeasured, and the single biggest
unknown in this territory.**

---

## 12. Stale references

- **`docs/airplay-fallback-design.md` — present on disk, absent from git.** `receiver.rs:301` and
  `CLAUDE.md:373` both cite it, and the file is in the working tree at `docs/`. But `git ls-files
  docs/` is empty: it is **untracked**, and a peer agent's
  `findings/major-docs-airplay-design-doc-only-in-dangling-commit.md` establishes that its only
  revision in the object database lives in a dangling commit. So the citation is not stale in the
  ordinary sense — the document is real and substantive — but it is one `git gc` from becoming
  stale. Deferring to that finding; no separate one filed here.
- **`mod.rs:1` vs `mod.rs:3-4` vs the rest of the codebase.** The module title says "macOS 10.13–10.14,
  where `CGVirtualDisplay` does not exist"; the next paragraph says the classes "arrived with 10.15";
  `virtual_display.rs`'s doctrine and `CLAUDE.md` both say 10.14.4 ("the classes predate Sidecar by a
  release"). Three different floors in the same subsystem. The code is right (it probes, it does not
  version-check); the doc comment is wrong twice.
- **`topology.rs:91-109`** — one doc comment on `ModeSnapshot` containing two contradictory
  paragraphs. The first says "The whole `CGDisplayMode` is kept, not just its size"; the second says
  "Stored as `io_display_mode_id`, not the `CGDisplayMode` object". The second is correct
  (`ModeSnapshot(Vec<(u32, i32)>)`, `topology.rs:111`); the first is a stale earlier draft.
- **`mod.rs:202-205`** — "Two hard requirements: an Accessibility grant … and a logged-in GUI session".
  Only the first is checked.
- **`fairplay.rs:16-17`** — "request dispatch keyed on the message type/sequence bytes instead of on
  body length". It dispatches on body length; the sequence half of every pattern is unreachable.
- **`ax.rs:351`** — `DISPLAYS_PREF_IDENTIFIER` is documented as "the cheapest way to recognise the
  Displays extra among the others", but `find_displays_extra_once` recognises it by `AXDescription`
  (`ax.rs:493-500`) and the constant is referenced only from a test.
- **`mirror.rs:181-183`** — "The sender expects *us* to poll *its* timing port; nothing polls ours"
  immediately followed by "the sender sends replies there". Both cannot be the operative reason for
  binding it.
- **`sender_prefs.rs`** — the module doc (`:1-21`) describes a module that deliberately writes nothing,
  while the file still contains `write()`, `enable_exact_geometry()`, the `Previous` enum, the
  `saved()` slot and two `CFPreferences` *write* FFI declarations. The doc is current; the code is the
  stale part.
- **No references to AI tooling anywhere in the territory.** Checked.
- The `§`/`M1`/`M2` markers the brief asks about do not appear here.

---

## 13. Dead code candidates

Full inventory with per-item evidence is in `findings/minor-airplay-dead-code-inventory.md`. Summary —
thirteen `pub` items with no caller outside the module or its own tests, none of which trip
`dead_code` because they are `pub` in a `pub mod`:

`mod.rs::probe()`, `ax::snapshot()` (only reachable from `probe`), `MenuSnapshot::offers_extend()`,
`ax::DISPLAYS_PREF_IDENTIFIER`, `sender_prefs::{enable_exact_geometry, write, Previous, saved}`,
`sender_prefs::restore()` (called but an unconditional no-op), `Receiver::geometry_changed()`,
`Receiver::idle_for()` (and the `State::last_contact` field that exists only to feed it),
`Inner::saw_teardown` (write-only), `mirror::CONNECT_GRACE`, `topology::is_online()`,
`Baseline::newcomers()`.

Two of these are worth *connecting* rather than deleting: `probe()` is what `ScreenExtend doctor` was
meant to report for this tier (CLAUDE.md says so; `cli.rs` never calls it), and `geometry_changed` is a
free push notification that macOS moved the display, which today is only discoverable by polling
`CGDisplayPixelsWide`.

---

## 14. Capability probing

**What this module gets right:** it probes rather than version-checks, in the places that matter.
- Selection of the tier itself is `AnyClass::get("CGVirtualDisplay")` (`virtual_display.rs:334-361`),
  not a version comparison — and it even logs when `CDVirtualDisplayCreate` is `dlsym`-able as a hint
  for a future direct path (`virtual_display.rs:353-358`).
- `ax.rs` handles the 10.13/10.14 vs 10.15 `AXIdentifier` split by *fallback chain*, not by version:
  `find_row` treats the identifier as a bonus confirmation and the title as the primary key
  (`ax.rs:536-569`), and `disconnect()` tries the bundle-localized title first and the positional
  heuristic second (`ax.rs:809-826`). The titles themselves come from `Displays.menu`'s own bundle
  (`ax.rs:157-189`), so no English is hardcoded. This is exemplary.
- `topology.rs` identifies the display by vendor/model fourcc rather than by list position or id.
- `menu_string` degrades to `None` on a bundle-key miss rather than matching the key literal
  (`ax.rs:186-187`).

**Where it does not, and what tier is missing:**
1. **`LEGACY_PAIRING: bool = false` is a `const`** (`receiver.rs:40`), and `/pair-verify` returns a
   terminal 403 (`receiver.rs:297-304`). There is no second tier: if a sender demands pairing, the
   session dies and the only artefact is a log line. The module's own comment says flipping the
   constant "is the first thing to try" — which means the experiment is known, understood, and not
   runnable without a rebuild. Filed as
   `findings/minor-airplay-pairing-and-model-are-compile-time-constants-not-probes.md`.
2. **`MODEL = "AppleTV2,1"` / `SOURCE_VERSION = "220.68"` / `FEATURES_BASE = 0x527FFEE6`** are frozen
   constants chosen because they are "field-proven against a macOS sender on this OS era"
   (`dnssd.rs:334-338`). That is a version-pinned guess wearing a capability's clothes: it is tuned to
   one OS generation and cannot adapt.
3. **`sender_prefs::FIXED_REFRESH_HZ = 60` and `MAX_WIDTH/MAX_HEIGHT = 1920x1080`** are asserted as
   universal facts about AirPlay displays (`sender_prefs.rs:31-45`) based on measurement on one
   machine. The *actual* ladder is queryable at runtime — `topology::available_modes(id)` does exactly
   that — so the clamp could be derived from the display instead of hardcoded. It cannot be derived
   *before* the display exists, which is the honest reason it is a constant; but nothing revises it
   afterwards, so a machine offering a richer ladder is silently capped.
4. **`availability()` does not probe the GUI session** even though `ax::system_ui_server_pid()` is
   exactly that probe and already exists. Filed as
   `findings/minor-airplay-availability-check-is-incomplete-and-ignored.md`.
5. **`maxFPS = 1` is asserted, never verified.** There is no probe, no counter check, and no log that
   would tell us whether macOS honours it. Given the CPU/GPU contention implication (§11) this is the
   most valuable missing measurement in the territory.

---

## 15. The AirPlay research question

### (a) Could this receiver run on modern macOS as a lower-latency display and/or frame source?

**As a display: no, and it is moot.** `probe_display_backend()` selects `CgVirtualDisplay` whenever
the private classes resolve, which is 10.14.4 onward (`virtual_display.rs:334-361`). On any modern
release the AirPlay tier is never chosen. Even if forced, `CGVirtualDisplay` gives instant, synchronous,
uncapped-count display creation at an exact geometry; the AirPlay path gives one display, negotiated
geometry off a fixed ladder capped at 1920x1080/60, after a 10–20 s handshake involving four
WindowServer reconfigurations and a driven menu bar. There is no dimension on which it competes.

**Would the handshake even complete?** Probably not. `/pair-verify` is a hard 403
(`receiver.rs:297-304`) and `LEGACY_PAIRING` is compiled off. The advertised identity is deliberately
an `AppleTV2,1` running `srcvers 220.68` — chosen *specifically* to keep the sender on legacy code
paths (`dnssd.rs:334-338`). A modern sender that insists on AirPlay-2 pairing (SRP6a + Ed25519 +
ChaCha20-Poly1305) gets refused at the first `/pair-verify`. This has never been tested on a modern
release; it is a ten-minute experiment on any Mac and nobody has run it. **That is the single cheapest
open question in this territory.**

**As a frame source: no, for a structural reason.** The frames would come from macOS's own AirPlay
sender pipeline — an encoder we cannot configure at all. Today the streamer owns QP, profile,
intra-refresh, keyframe cadence and a loss-driven bitrate controller (`bitrate.rs`,
`webrtc_session.rs`); the AirPlay sender exposes none of that and is tuned for a wireless link to an
Apple TV, including its own buffering. We would trade a pipeline controlled end to end for one we
cannot touch, in exchange for removing a capture and an encode from *our* budget — while macOS does
that same work anyway. The only genuine saving would be avoiding a second encode of the same content,
and that is only real if we forward the H.264 verbatim, which brings us to (b).

### (b) What would it take to decode the incoming H.264?

**Is the stream we get encrypted? Yes — the payloads. The headers and the codec data are not.**

- Packet type `0x01` (SPS/PPS, AVCC) is explicitly **plaintext**: `mirror.rs:16-18` calls it
  "unencrypted SPS/PPS", and it is re-sent on every geometry change. So profile, level and dimensions
  are readable right now with no crypto at all. Nothing reads them — `drain()` only sets a latched
  flag (`mirror.rs:128-131`).
- The 128-byte header is plaintext; only `header[0..4]` and `header[4]` are read (`mirror.rs:120-136`).
- Type `0x00` video payloads are AES-128-CTR ciphertext. `receiver.rs:18-20` names the inputs exactly:
  "`ekey`/`eiv` from the first SETUP and `streamConnectionID` from the second are the inputs to AES-CTR
  video decryption, and we parse neither."

**What the static tables imply.** They imply we have deliberately stopped one step short of the crypto.
`fairplay.rs:1-10` is precise: message 1 selects one of four canned 142-byte replies by `req[14]`;
message 2 returns a fixed 12-byte header plus an echo of `req[144..164]`; "No key material, no session
state, no randomness. The real FairPlay work only begins at `fairplay_decrypt`, which recovers the AES
key for the video stream — and we never decode video, so we never go there." The tables are a **replay
of a captured legitimate handshake** — enough to make the sender proceed, and structurally incapable of
producing a key. Version 3 is the only version with published tables; a version-2 message exists in the
wild with no published reply (`fairplay.rs:92-93`).

So decoding requires importing playfair's `fairplay_decrypt` — the reverse-engineered key-derivation
over the 72-byte `ekey` blob, plus the per-stream key schedule from `streamConnectionID`, plus AES-CTR.
That is a materially larger and more legally exposed blob than the four reply tables (which are used
here under LGPL-2.1-or-later via GPL-3/AGPL-3 §13, attributed at `fairplay.rs:12-17`). Decrypting and
then *re-encoding* is strictly worse than capturing the display we already have; decrypting and
forwarding verbatim is the only interesting version, and it is the one that needs the most crypto.

**Recommendation: do not pursue decryption. Do pursue the free half** — read the plaintext type-`0x01`
codec data (it tells us what geometry macOS thinks the display is, without polling CoreGraphics) and
the plaintext header timestamps (see (c)). Both are pure reads of bytes already arriving.
**One check first**, before anyone writes a line of crypto: dump 64 bytes of a type-`0x00` payload on
the 10.15 box. If a NAL start code or an AVCC length prefix is visible in the clear — which is
possible, since we advertise no encryption support in a way any of this code verifies — the entire
assessment changes and the work becomes cheap. Ten minutes, never done. Filed as
`findings/speculative-airplay-decoding-the-mirror-stream-needs-real-fairplay.md`.

### (c) What timing/latency information does the sender expose?

More than we use. Everything below already arrives and is discarded:

| Signal | Where it arrives | What we do with it |
|---|---|---|
| **NTP-format capture timestamp, per frame** (header offset 8 in every published receiver) | `mirror.rs:114` reads all 128 header bytes | reads bytes 0–4 only; **123 bytes discarded per packet** |
| payload-option / flag field (header offset 6) | same | discarded |
| **type `0x05` stream reports** (sender's own encoder/bandwidth statistics) | `mirror.rs:39, 132` | counted and skipped |
| **`streamConnectionID`, `latencyMs`, `timestampInfo`, `spf`** in the streams SETUP plist | `info::requested_stream_types` (`info.rs:183-198`) | only `streams[].type` is read |
| **the sender's `timingPort`** in the keys/timing SETUP | `info::is_timing_setup` (`info.rs:202-208`) | only tests for `ekey`/`eiv` presence; the port is never read |
| our own timing port (an NTP-like exchange the sender replies on) | `TimingSocket` (`mirror.rs:184-205`) | bound, set non-blocking, **never read** |
| `POST /feedback`, every ~2 s | `receiver.rs:350` | bare 200; body discarded |
| `refreshRate`, `maxFPS`, `overscanned`, `features` | we *send* these (`info.rs:106-126`) | `maxFPS = 1`, effect unverified |

**Why this is the most valuable thing in the territory.** The AirPlay sender is *the same machine*, so
its NTP timestamps and our `streamer::audio::host_now_ns()` share a hardware timebase
(`mach_absolute_time`). That makes the mirror stream a free, already-connected instrument for
something nothing else in the codebase measures: **how long macOS itself takes between compositing a
frame and handing it to a consumer.** That number is the floor for *any* capture path on macOS —
CGDisplayStream and ScreenCaptureKit alike — and if it turns out to be large it reframes the entire
macOS latency budget for this engagement. The experiment is: hexdump 32 header bytes to confirm the
layout, then histogram `host_now_ns() - ntp_to_host_ns(header[8..16])`, with `DISCARD_FPS` temporarily
raised behind an env flag. Filed as
`findings/speculative-airplay-sender-timestamps-are-received-and-discarded.md`.

### (d) Which parts are reusable for an AWDL / peer-to-peer transport experiment?

**Reusable now:** `dnssd.rs`'s FFI (the only Bonjour binding in the tree — `DNSServiceRegister`, the
`TXTRecord*` family, RAII withdrawal, arbitrary regtype/port/interface/TXT), `rtsp.rs` minus the RTSP
specifics (a small dependency-light framed request/response reader with explicit ceilings),
`mirror.rs`'s framing loop and its `setsockopt` keepalive block (the only place in the tree that sets
`TCP_KEEPALIVE` on a data socket), and `Cancel`.

**Not reusable:** `ax.rs`, `topology.rs`, `fairplay.rs`, `info.rs`, `sender_prefs.rs`, `receiver.rs`.

**The gap, and it is a real one.** `primary_interface_index()` explicitly skips AWDL —
`if name.starts_with("awdl") || name.starts_with("llw") || name.starts_with("utun") { continue; }`
(`dnssd.rs:100-104`). For AirPlay that is correct. For an AWDL experiment, inverting it would *not*
work, because on macOS AWDL is not selected by interface index — it is enabled by Bonjour flags this
FFI does not bind: `kDNSServiceFlagsIncludeAWDL` (0x100) and `kDNSServiceInterfaceIndexP2P`
(0xFFFFFFFE). Neither constant appears anywhere in the repo. The only flag constant defined is
`KDNS_SERVICE_FLAGS_NO_AUTO_RENAME`, and it is wrong (0x0 instead of 0x8 — a separate major finding).

**The shape that could actually work is the opposite of "AirPlay over AWDL".** ScreenExtend's clients
are browsers, and a browser cannot open an AWDL socket. The plausible experiment is: bring AWDL up
between host and an Apple client, bind the existing axum HTTPS + WebRTC listener to the `awdl0`
address, and advertise it with `kDNSServiceFlagsIncludeAWDL` so Safari on the peer reaches it without
either device joining an AP. `Service::register` already takes an interface index, so the plumbing is
most of the way there. Three real unknowns: whether Safari on iOS will connect to a link-local host
over `awdl0` at all; whether AWDL stays up without a system service holding it (it is normally brought
up on demand by AirDrop/Sidecar and torn down aggressively); and whether it is even faster — AWDL
time-shares the radio with the infrastructure link and its availability windows add jitter, so it is
**not obviously a win** and should be measured before any code is written. Filed as
`findings/speculative-airplay-dnssd-ffi-is-reusable-for-an-awdl-transport.md`.

---

## 16. Questions and cross-cutting concerns for the orchestrator

1. **Is this tier still in scope?** It only runs below macOS 10.14.4 — below the app's own
   `minimumSystemVersion` of 10.15 (`tauri.macos.conf.json`, CLAUDE.md). So the shipped binary refuses
   to launch on every OS where this backend would be selected. `mod.rs`'s title even says
   "10.13–10.14". If that is right, the entire territory is unreachable in production and the blocker
   below is theoretical. **This needs a decision before anyone spends time here** — and it should be
   written down in `mod.rs`, because nothing in the module says it.
2. **One blocker.** `MirrorSink::run` serves exactly one connection, so the second AirPlay session of a
   process's life advertises a dead data port. The module's own live harness tests for exactly this
   and would catch it on the 10.15 box.
3. **Cross-territory:** the AirPlay path deliberately avoids `CGDisplaySetDisplayMode` (measured: 48 s
   of display persistence), and then `server.rs:975-977` unconditionally calls it via
   `pipeline::set_display_mode` → `virtual_display.rs::activate_mode` on the very display it just
   created. Owner is ambiguous — it is a `server.rs` decision breaking a `topology.rs` invariant.
4. **Cross-territory:** 13 log/error strings in this module contain 6–18 embedded space runs from
   dropped `\` continuations, and a stricter repo-wide grep finds 11 more elsewhere. `cargo fmt`
   cannot see inside string literals and there is no lint. Worth one mechanical pass across all
   territories plus a grep guard, since CI is disabled.
5. **Cross-territory (small but sharp):** `ax.rs:88-90` documents that its hand-declared
   `CFBooleanGetValue` signature must match the one in `macos_utils/streamer/encoder.rs` or the linker
   clashes. Hand-declared CF/CG FFI is spread across at least three modules with no shared header.
   Someone will eventually declare the same symbol with a different signature and get a confusing link
   error. A single `macos_utils/cf_sys.rs` would end the class of bug.
6. **The one measurement I want and could not take:** does macOS honour the advertised
   `maxFPS = 1`? If not, a live AirPlay-tier session means the host is encoding its entire desktop at
   full rate into a socket we discard, competing for the GPU with our own VideoToolbox encode. That is
   the difference between "this tier is slow to join" and "this tier halves your framerate". It needs
   one `tprintln!` of `stats.packets` per second on the 10.15 box.
7. **The other one:** the mirror header's NTP timestamp would give us macOS's own compositor-to-consumer
   latency for free (§15c). I would put this ahead of most of the fixes above in value-per-hour, and it
   is read-only.
8. **Note for whoever synthesises `RISK-REGISTER.md`:** the dominant risk in this territory is not a
   bug, it is silence. A fake AirPlay receiver fails by macOS quietly not creating a display, and §9
   lists ~20 places where the evidence that would explain it is discarded. `diagnose()` is the one
   mechanism built to fight that, and it reports the wrong answer after the first session
   (`findings/minor-airplay-phase-latches-to-ended-so-diagnose-misreports.md`).

**Findings written: 20** (1 blocker, 5 major, 11 minor, 3 speculative), all in `findings/`, all
prefixed `*-airplay-*`. Two other `airplay`-named files in that directory
(`major-docs-airplay-design-doc-only-in-dangling-commit.md`,
`speculative-airplay-relay-rejected.md`) are peers' and are not mine.
