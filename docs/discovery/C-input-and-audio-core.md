# C — Input and audio core

Territory: `src-tauri/src/streamer/input/` (all 7 files) and `src-tauri/src/streamer/audio/`
(all 5 files). Read in full. Call sites in `streamer/webrtc_session.rs`,
`streamer/static/input.js` and the two OS audio backends were read only as far as needed to
close the paths; they belong to other territories and are marked as such.

---

## 1. Files and purpose

### `streamer/input/`

**`mod.rs`** (127 lines) — the module's public face and its threading model. Declares
`DisplayRect` (the target display's screen rect), a `Stats` counter pair, the per-OS
`#[path]`-selected `backend` module, and the injector thread. `spawn()` creates two
crossbeam channels — a **bounded 512-deep "fast"** channel and an **unbounded "reliable"**
one — plus an OS thread named `injector` that boosts its own priority, constructs the
backend `Injector`, and merges the two channels with `crossbeam::select!`. `InjectorTx` is
the cloneable send handle; `route(ev, hot)` picks the channel by the `hot` flag. Also exposes
`boost_current_thread()`, `tune_process()` and `prime()` (macOS-only body).

**`protocol.rs`** (620 lines, ~150 of them tests) — the entire client→host input wire format:
25 opcodes, source/modifier/button bitmasks, the `Phase` and `InputEvent` enums, a set of
little-endian fixed-offset readers, one `parse()` entry point returning
`(InputEvent, is_fast)`, and three host→client frame builders (`build_pong`, `build_bye`,
`build_stats`). Carries a file-level `#![allow(dead_code)]`, which is what hides several
orphaned surfaces from the compiler. Has the module's only unit tests.

**`scancode.rs`** (120 lines) — a single pure function mapping W3C `KeyboardEvent.code`
strings to `(PS/2 set-1 scancode, is_extended)` for Windows `SendInput`. Data only; no state,
no OS calls. Covers the standard 104-key layout plus `IntlBackslash`/`IntlRo`/`IntlYen`.

**`windows.rs`** (925 lines) — the Windows backend and by far the largest file. Uses three
distinct injection APIs: `SendInput` for mouse/keyboard/text, `InjectSyntheticPointerInput`
over `CreateSyntheticPointerDevice` for real pen and multi-touch, and `PostMessageW`
+`WM_DROPFILES` for file drops. Owns virtual-screen geometry, per-contact touch state, held-key
tracking, clipboard writes via the Win32 clipboard API, and the coordinate transforms between
normalized client space and the virtual desktop.

**`macos.rs`** (785 lines) — the macOS backend, built entirely on hand-declared CoreGraphics
`CGEvent*` FFI plus a little `objc2` for the pasteboard. No pen or touch injection: both
degrade to mouse. Contains its own `code_to_keycode` table (macOS virtual keycodes, not
scancodes) with a hardcoded Ctrl↔Command remap, explicit modifier-flag bookkeeping, and
`prime_keyboard()`.

**`linux.rs`** (37 lines) — a compiling stub. Warns once, drops every event. Its header
comment is a genuinely useful uinput design note attached to a dangling "PRD §5.7" citation.

**`generic.rs`** (16 lines) — a no-op backend for any other target. Never compiled today.

### `streamer/audio/`

**`mod.rs`** (233 lines) — three separable things in one file: (a) **the shared host
timebase** (`HOST_EPOCH`, `host_now_ns`, `host_instant_to_ns`, `host_ns_to_rtp90k`) that both
audio capture and video RTP stamping ride; (b) the transport data types (`AudioPacket`,
`AudioFormat`, `AudioDiagnostics`, `AudioCapture`); (c) **`AudioHub`**, the reference-counted
one-capture-to-N-sessions fanout built on a `tokio::sync::broadcast` channel with a dedicated
crossbeam→broadcast bridge thread.

**`encoder.rs`** (182 lines) — the OS-independent libopus encoder wrapper. Creates the
encoder in `RESTRICTED_LOWDELAY`, applies seven CTLs, reads back lookahead, and encodes 5 ms
float frames into a reused 4000-byte scratch buffer.

**`opus_sys.rs`** (200 lines) — hand-written libopus FFI loaded at runtime via `libloading`.
Constants, function-pointer types, a multi-path library search (bundled resources, macOS
`.app` Resources, exe dir, loader path), and `OpusApi::load()`.

**`protocol.rs`** (106 lines) — the 13-byte DataChannel audio header (seq u32 / capture_ns
u64 / flags u8, little-endian) with write/build/parse and five unit tests.

**`tests.rs`** (168 lines) — a mock `AudioCapture` producer plus a `SeqGate` type that models
the client's dedup/reorder policy. Note `SeqGate` is **defined in the test file only**; no
production code uses it.

---

## 2. Function catalog

### `input/mod.rs`
- `boost_current_thread()` — delegates to `backend::boost_thread()`. **No callers anywhere.**
- `tune_process()` — delegates to `backend::tune_process()`. **No callers anywhere** (dead; see finding).
- `prime()` — calls `backend::prime_keyboard()` on macOS only; empty elsewhere. Called from `lib.rs:434` and `cli.rs:438`.
- `InjectorTx::route(ev, hot)` — `try_send` on the bounded fast channel (drop + counter on full) or blocking-free `send` on the unbounded reliable channel (result discarded).
- `InjectorTx::fast_queue_depth()` — `self.fast.len()`. **No callers.**
- `InjectorTx::release_all()` — sends a synthetic `Lifecycle::Focus(false)` down the reliable channel so the injector releases held buttons/keys. Called from `webrtc_session.rs:579` on peer-connection Failed/Disconnected/Closed.
- `spawn(device_name) -> (InjectorTx, JoinHandle)` — builds the channels and the injector thread; `.expect("spawn injector thread")` on failure.

### `input/protocol.rs`
- `Phase::from_opcode(op)` — maps the 8 pointer opcodes to phases; catch-all is `Phase::Move` (silently wrong for anything else).
- `is_fast(op)` — true for MOVE, MOVE_BATCH, ENTER, LEAVE, OVER, OUT, WHEEL, ZOOM, MOUSE_DELTA.
- `rd_u16/rd_i16/rd_u32/rd_u64/rd_f32(b, o)` — bounds-checked little-endian readers returning `Option`.
- `parse(b) -> Option<(InputEvent, bool)>` — opcode dispatch; unknown opcode → `None`.
- `parse_pointer` — requires ≥40 bytes; fixed layout op/src/id/x/y/pressure/tiltX/tiltY/twist/w/h/buttons.
- `parse_pointer_batch` — 10-byte header + N×24-byte samples; validates total length **before** allocating.
- `parse_zoom`, `parse_wheel` (≥15), `parse_mouse_delta` (≥8), `parse_drag` (≥10), `parse_resize` (≥9) — fixed-size decoders.
- `parse_key` — variable: flags u8, mods u16, u8-length `code`, u8-length `key`; both must be valid UTF-8; interned into `SmolStr`.
- `parse_text(b, opcode)` — u32 length + UTF-8; `composing` set for `COMPOSITION_UPDATE`.
- `parse_clipboard` — op u8, u32-length mime, u32-length data.
- `parse_drop` — x/y/count then per-item name/mime/size/data; allocates `Vec::with_capacity(count)` **before** validating (finding).
- `build_pong(t_ns) -> [u8;9]`, `build_bye(reason) -> [u8;2]`, `build_stats(...) -> [u8;25]` (**no callers**).

### `input/scancode.rs`
- `code_to_scancode(code) -> Option<(u16, bool)>` — one match arm per key; returns `None` for anything unmapped, which the caller uses as its signal to fall back to Unicode text injection.

### `input/windows.rs`
- `boost_thread()` — `SetThreadPriority(TIME_CRITICAL)`; result discarded.
- `tune_process()` — `timeBeginPeriod(1)` (return discarded, no `timeEndPeriod`) + `SetPriorityClass(HIGH_PRIORITY_CLASS)`. **Dead.**
- `VirtualScreen::query()` — five `GetSystemMetrics` calls with a single-monitor fallback.
- `Injector::new(device_name)` — creates the synthetic pen (1 contact) and touch (10 contacts) devices; each failure logs once and degrades to mouse.
- `Injector::refresh_geometry()` — re-queries the virtual screen and the target `monitor_rect` at most every 500 ms; called at the top of **every** `dispatch`.
- `Injector::dispatch(ev)` — the event switch.
- `Injector::release_all()` — releases all buttons, drains and releases every held key, lifts the pen, lifts every touch contact.
- `mouse_pointer(x,y,buttons,phase)` — the absolute-mouse path; returns early on Leave/Out, releases buttons on Cancel, sets `mouse_relative=false` only on Move/Enter/Over.
- `mouse_delta(dx,dy,buttons)` — relative `MOUSEEVENTF_MOVE`, **unclamped**.
- `wheel(dx,dy,mode)` — scales by `WHEEL_DELTA` per DOM delta mode; inverts dy.
- `pointer_batch(source,id,buttons,samples)` — loops per sample, one OS call each (finding).
- `zoom(delta)` — synthesizes Ctrl+wheel, skipping the Ctrl press if already held.
- `key(down, code, key)` — scancode path first; falls back to `text()` for a single non-control character; else `log::debug!`.
- `text(s)` — UTF-16 code-unit pairs as `KEYEVENTF_UNICODE` down+up; batched into `scratch`, one `SendInput`.
- `pen(...)` / `pen_info(...)` — `POINTER_PEN_INFO` with pressure×1024, twist, tilt; `pointerId` hardcoded to 1.
- `touch(...)` / `touch_info(...)` — maintains a `HashMap<u32, Contact>` and re-emits the **whole contact frame** on every change (correct for the API); synthesizes 512 pressure when in contact and none reported.
- `clipboard(op,mime,data)` / `paste_hotkey()` — writes then optionally injects Ctrl+V.
- `drop_files(x,y,items)` — writes each item to `%TEMP%\input-bridge-drop`, builds a `DROPFILES` global, `WindowFromPoint` + `ScreenToClient`, `PostMessageW(WM_DROPFILES)`.
- `lifecycle(l)` — Focus(false)/Visibility(false) → `release_all()`; **PointerLock ignored** (finding).
- `abs`, `pixel`, `inject_point`, `space_dims` — the coordinate transforms. `abs` maps to the 0–65535 virtual-desktop space; `inject_point` maps to virtual-desktop-relative pixels (what the pointer-injection API wants); `pixel` maps into the target display.
- free functions: `button_transitions(old,new,out)` (XOR-diff to press/release events), `ext_flag`, `mouse_input`, `key_input`, `send_inputs` (one `SendInput`, warns on partial), `inject_pointer`, `set_clipboard`, `set_clip_text`, `set_clip_raw`, `put_clipboard`, `bytemuck_u16`.

### `input/macos.rs`
- `modifier_self_flag(vk)` — maps a modifier keycode to its own CGEventFlags bit, used to avoid setting a modifier's flag on its own key-down.
- `code_to_keycode(code)` — the macOS virtual-keycode table; **hardcodes ControlLeft→Command and MetaLeft→Control** (finding).
- `boost_thread()` — `macos_utils::streamer::qos::pin_current_thread_user_interactive()`.
- `tune_process()` — empty.
- `Injector::new(device_name)` — creates a HID-state `CGEventSource`, sets the local-events suppression interval to 0 (correct — otherwise synthetic moves suppress the real mouse for 0.25 s), warns if Accessibility is not trusted **but proceeds anyway**, seeds `pos` from the current cursor location.
- `Injector::refresh_geometry()` — same 500 ms cadence as Windows; recomputes `desktop_bounds()` and the target rect.
- `Injector::dispatch/release_all` — as Windows.
- `mouse_pointer`, `mouse_delta` (clamped to the whole desktop, not the target), `motion` (picks MOVED vs LEFT/RIGHT/OTHER_DRAG from the button state), `button_transitions`, `post_mouse` (creates, sets button number / deltas / flags, posts to `kCGHIDEventTap`, releases).
- `wheel(dx,dy,mode)` / `zoom(delta)` — both call the **mis-declared** `CGEventCreateScrollWheelEvent` (blocker finding).
- `key(down,code,key)` — updates `down_keys`, recomputes modifier flags, and deliberately excludes a modifier's own bit from its key-down event.
- `recompute_mods()` — derives CGEventFlags from the held-key set.
- `post_key(keycode,down,flags)`, `text(s)` (posts keycode 0 down+up with a Unicode string attached).
- `pen`, `touch`, `pointer_batch` — all degrade to `mouse_pointer`.
- `clipboard`, `paste_hotkey` (Cmd+V), `lifecycle` (**does** honour PointerLock), `unsupported_drop` (warns once), `point(nx,ny)`.
- `Drop for Injector` — `CFRelease`s the event source. (No Windows equivalent — see the handle-leak finding.)
- `current_mouse_location()`, `prime_keyboard()`, `desktop_bounds()`, `set_pasteboard(mime,data)`.

### `audio/mod.rs`
- `host_now_ns()` — nanoseconds since a lazily-initialized `Instant` epoch.
- `host_instant_to_ns(instant)` — the same epoch applied to a caller-supplied `Instant`, via `saturating_duration_since`.
- `host_ns_to_rtp90k(ns)` — `ns × 90000 / 1e9`, truncated to u32 (wraps every ~13.25 h; expected for RTP).
- `AudioDiagnostics::summary()` — one formatted line; called by both capture backends, not by the forwarders.
- `AudioHub::new()`, `subscribe()`, `unsubscribe()`, `is_running()`, `diagnostics()` — see §4 and §6.
- `Debug for AudioHub` — takes the mutex inside `fmt` (a `Debug` impl that can block; harmless but notable).

### `audio/encoder.rs`
- `OpusEncoderConfig::default()` — 48 kHz, 2ch, 128 kbps, complexity 5, 240 samples (5 ms).
- `OpusEncoder::new` / `with_api` — validates channels ∈ {1,2} and rate == 48 kHz, creates the encoder in `RESTRICTED_LOWDELAY`, applies the seven CTLs, reads lookahead.
- `ctl_set` / `ctl_get` — **both check the return code and `bail!` on non-OK.** Good.
- `encode_float(pcm)` — length-validated, encodes into the reused scratch, returns a borrowed slice. No per-frame allocation.
- `channels()`, `frame_samples()`, `lookahead_samples()`, `version()` (**never called**).
- `Drop` — `opus_encoder_destroy`.

### `audio/opus_sys.rs`
- `open_library()` — tries `<exe>/resources/<name>`, `<exe>/../Resources/resources/<name>`, `<exe>/../Resources/<name>`, `<exe>/<name>`, then the bare name on the loader path, per candidate name.
- `OpusApi::load()` — resolves 9 symbols (note `opus_encoder_ctl` is resolved **twice** into two identically-typed variadic pointers, `encoder_ctl_set` and `encoder_ctl_get` — a readability device, not a functional one).
- `OpusApi::version()`, `OpusApi::strerror(code)` — null-checked `CStr` conversions.

### `audio/protocol.rs`
- `write_message(out, seq, capture_ns, flags, opus) -> usize` — writes the 13-byte header + payload into a caller buffer; size guarded only by `debug_assert!`.
- `build_message(...) -> Vec<u8>` — allocating convenience wrapper (used only in tests).
- `AudioHeader::is_silent()`, `is_discontinuity()`.
- `parse(b) -> Option<(AudioHeader, &[u8])>` — host-side decoder; **no production caller** (the client decodes this in `audio-worker.js`), used by the tests.

---

## 3. Threads

| Thread | Spawned at | Priority / QoS | Lifetime | Owns |
|---|---|---|---|---|
| `injector` | `input/mod.rs:96-117`, one **per WHEP session** (`webrtc_session.rs:394`) | Windows `THREAD_PRIORITY_TIME_CRITICAL` (`windows.rs:69-73`); macOS user-interactive QoS (`macos.rs:236-238`); Linux/generic none | Detached (`JoinHandle` bound to `_input_join` and dropped). Exits when **either** channel disconnects (`select!` breaks on the first `Err`) | The backend `Injector`: geometry cache, `buttons`, `down_keys`, `contacts`, `pen_down`, `mouse_relative`, the scratch vectors, and on Windows the two `HSYNTHETICPOINTERDEVICE` handles |
| `audio-bridge` | `audio/mod.rs:156-162`, one per hub start | **None — plain `std::thread`, default priority** | Started on first subscriber, joined under the hub mutex on last unsubscribe | Nothing; it is a pure copy loop from the crossbeam receiver into the broadcast sender |
| (capture thread) | `platform::start_audio_capture()`, out of territory | Windows MMCSS "Pro Audio"; macOS RT callback + worker | Owned by the `AudioStopFn` | The `crossbeam::Sender<AudioPacket>` |
| (test producer) | `audio/tests.rs:16` | none | joined by the mock `stop` closure | test only |

The injector thread is the only place the OS injection APIs are touched, so all backend state
is single-threaded by construction — there is no lock inside any `Injector`.

**Serialization point:** `crossbeam::select!` (`mod.rs:103-112`) picks **randomly** among
ready operations. It is not priority-ordered, so a reliable-channel key-down has no precedence
over a fast-channel pointer move, and cross-channel arrival order is not preserved.

---

## 4. Locks

| Lock | Guards | Taken by | Hot path? | Across `.await`? |
|---|---|---|---|---|
| `AudioHub::inner: std::sync::Mutex<Option<HubInner>>` (`audio/mod.rs:118`) | `running: Option<Running>` + `refcount` | `subscribe` (:147), `unsubscribe` (:194), `is_running` (:215), `diagnostics` (:224), `Debug::fmt` (:125) | **No** — per subscribe/unsubscribe, not per packet | **No** `.await` inside, but the guard is held across `platform::start_audio_capture()` (a blocking device open), across `stop()`, and across `bridge.join()` — see the finding. Both call sites wrap it in `spawn_blocking` (`webrtc_session.rs:729`, `:741`), so the `!Send` compile guard is satisfied and the blocking is legal, merely serializing. |
| `broadcast` internal `RwLock` (tokio) | the fanout ring | bridge thread `send`, each forwarder `recv` | **Yes**, 200/s per session | Handled by tokio; `send` is sync and non-blocking |
| `Arc<Stats>` atomics (`mod.rs:20-21`) | two `AtomicU64` | `route` | Yes | n/a — `Relaxed`, and nothing ever reads them |
| `control_rx: Arc<Mutex<...>>` (`webrtc_session.rs:397`) | the kick-message receiver | the `on_data_channel` closure (:410) | No, once per channel open | `.lock().unwrap().take()` inside a `.then(...)` — the guard is a temporary that ends at the semicolon, and the `.await` is after; **safe, but only just** |

**Temporary-lifetime scrutinee check (the `if let` footgun).** I looked for the pattern
`if let Some(x) = <lock>.lock().unwrap().<...>` with an `.await` in the body across both
directories and the call sites. The instances that exist are all safe:

- `audio/mod.rs:150` `if inner.running.is_none()` — `inner` is a plain `&mut` borrow of an
  already-held guard, no `.await` anywhere in the function.
- `audio/mod.rs:202` `if let Some(mut running) = inner.running.take()` — `take()` moves the
  value out; the guard is held for the whole block but there is no `.await`, only blocking
  calls. This is the "long critical section" problem, not the "guard across await" problem.
- `audio/mod.rs:123-128` (`Debug::fmt`) uses `.lock().ok().and_then(...)`, which drops the
  guard at the end of the statement.
- `webrtc_session.rs:409-411` — as above, guard dropped before the `if let Some(mut rx)`.

No `MutexGuard`-across-`.await` violation found in this territory.

---

## 5. Buffers and copies

**Input, client → OS.** Per message, in order:
1. `msg.data: Bytes` arrives from webrtc-rs — already owned, refcounted (no copy from SCTP).
2. `parse` reads fields out of the slice by value. **CPU copy, tiny**, into the `InputEvent`.
   `SmolStr` (`protocol.rs:161-162`) keeps `code`/`key` inline for ≤22 bytes, so the common
   key event allocates nothing.
   - `PointerBatch`: **one heap allocation** of `Vec<MoveSample>`, up to 40×24 = 960 B.
   - `Text`/`Clipboard`/`Drop`: `String`/`Vec<u8>` allocations sized by the message.
3. `InputEvent` is **moved** into the crossbeam channel — no copy of heap contents, but the
   enum itself (largest variant ~48 B) is memcpy'd into the channel slot.
4. Injector thread: `InputEvent` moved out; `dispatch(&ev)` takes it by reference.
5. Windows: fields are packed into `INPUT` structs in the **reused** `self.scratch: Vec<INPUT>`
   (`windows.rs:134`, `with_capacity(16)`) — **no allocation in steady state**, one `SendInput`
   copies into kernel space. Touch frames use the reused `self.touch_scratch`, moved in and out
   with `std::mem::take` to satisfy the borrow checker (`windows.rs:539`, `:559`) — a pointer
   swap, not a copy.
6. macOS: each event is a fresh CoreFoundation object (`CGEventCreateMouseEvent` /
   `CreateKeyboardEvent`) — **one CF allocation per event**, released immediately after
   `CGEventPost`. There is no reuse path; the API has no batch form.

Notable: `drop_files` (`windows.rs:624-690`) writes the payload to disk (`std::fs::write`),
then copies the UTF-16 path list into a `GlobalAlloc` block — two extra full copies, but this
is a cold path.

**Audio, capture → client.** Per 5 ms packet:
1. Capture backend produces `AudioPacket { data: Bytes }` — one allocation + copy out of the
   Opus scratch (in the backend, out of territory).
2. crossbeam `send` → `recv`: **move**, no copy.
3. Bridge thread `broadcast::send`: the `AudioPacket` is **cloned into the ring**; `Bytes` clone
   is a refcount bump, so this is cheap, but the struct is copied per subscriber slot.
4. Each subscriber's `recv()` — another `AudioPacket` clone (refcount bump).
5. `forward_datachannel` (`webrtc_session.rs:784-792`): `scratch.resize()` (zero-fills growth)
   → `write_message` **memcpy #1** of the Opus payload → `Bytes::copy_from_slice(&scratch)`
   **allocation + memcpy #2**. Both avoidable; see the finding.
6. `dc.send()` → SCTP.

The encoder itself is clean: `encode_float` writes into the preallocated 4000-byte
`self.scratch` (`encoder.rs:85`, `:142-154`) and returns a borrow — no per-frame allocation.

---

## 6. Queues

| Queue | Depth | Bounded | On full | Back-pressure |
|---|---|---|---|---|
| input `fast` (`mod.rs:92`) | **512** (`FAST_CAP`) | Yes | `try_send` fails → **event dropped**, `fast_dropped` incremented, nothing logged | None. At 5 ms cadence 512 entries ≈ 2.5 s of stale pointer history that is **replayed in order**, not skipped. |
| input `reliable` (`mod.rs:93`) | unbounded | No | n/a | None; `send` result discarded (`mod.rs:74`) |
| audio capture→bridge crossbeam (`AudioCapture.rx`) | set by the backend (out of territory) | unknown | unknown | unknown — worth confirming in territories D/E |
| audio broadcast (`audio/mod.rs:93`) | **128 packets = 640 ms** | Yes | slow receiver gets `RecvError::Lagged(n)`; the producer never blocks | Consumers `continue` on `Lagged`, discarding `n` and **not** setting `FLAG_DISCONTINUITY` |
| SCTP send buffer (audio DC) | — | — | forwarder drops packets when `buffered_amount() > 16 KiB` (~1 s of audio) | Sampled 1-in-4 packets; drops are silent to the client |

Both audio queues are sized in packets and both convert to far more milliseconds than a
real-time path wants.

---

## 7. Syscall / FFI boundaries

**Windows** (`windows.rs`), all via the `windows` crate, standard `extern "system"`:
`SetThreadPriority`, `SetPriorityClass`, `timeBeginPeriod`, `GetSystemMetrics` (×6 per
geometry refresh), `CreateSyntheticPointerDevice`, `InjectSyntheticPointerInput`, `SendInput`,
`OpenClipboard`/`EmptyClipboard`/`SetClipboardData`/`CloseClipboard`/`RegisterClipboardFormatW`,
`GlobalAlloc`/`GlobalLock`/`GlobalUnlock`/`GlobalFree`, `WindowFromPoint`, `ScreenToClient`,
`PostMessageW`. **`DestroySyntheticPointerDevice` is never called** (finding).
`SendInput` is passed `size_of::<INPUT>()` explicitly (`windows.rs:850`) — correct
versioning discipline. `POINTER_TYPE_INFO`/`POINTER_PEN_INFO`/`POINTER_TOUCH_INFO` are built
with `..Default::default()` for the unset `POINTER_INFO` fields, which is the right way to
handle a struct whose layout the crate owns.

**macOS** (`macos.rs:17-50`) — a hand-written `#[link(name = "CoreGraphics", kind = "framework")]`
block with 12 declarations. **Dyld exposure:** every symbol declared here is link-time bound.
All are ancient (10.0–10.4 era: `CGEventCreateMouseEvent`, `CGEventPost`,
`CGEventKeyboardSetUnicodeString`, `CGEventSetIntegerValueField`, …), so this does **not**
violate the CLAUDE.md "no link-time references newer than 10.15" rule — I checked each name
against its availability and found nothing newer than 10.6. The pasteboard uses runtime
`objc2` (`class!`/`msg_send!`, `macos.rs:752-784`), so no Objective-C class is bound at link
time either. This is compliant.

**One ABI defect:** `CGEventCreateScrollWheelEvent` (`macos.rs:33-38`) is declared with
`wheel1` inside the variadic part, but the real prototype has it as a **named** parameter. On
arm64 Darwin named args go in registers and variadic args go on the stack, so the callee reads
`wheel1` from a register the caller never wrote. Filed as a blocker.

**libopus** (`opus_sys.rs`) — loaded at runtime with `libloading`, so nothing is bound at link
time and a missing library is a clean `Err` rather than a load failure. Nine symbols resolved.
`opus_encoder_ctl` is genuinely variadic in `opus.h` and is declared correctly here — the
contrast with the CoreGraphics case above is instructive. All request numbers, application
constants, signal constants and error codes were checked against `opus_defines.h` and are
correct. `OpusEncoder` is an opaque `[u8; 0]` — right, since the struct is private to libopus.
`unsafe impl Send`/`Sync for OpusApi` and `unsafe impl Send for OpusEncoder` are sound given
the encoder is only ever touched from one thread.

---

## 8. Panics

| Site | Trigger |
|---|---|
| `input/mod.rs:117` `.expect("spawn injector thread")` | thread-spawn failure (resource exhaustion) aborts the WHEP negotiation |
| `audio/mod.rs:147,194,215,224` `.lock().unwrap()` | mutex poisoning — reachable if any `.expect` below fires while the lock is held |
| `audio/mod.rs:148,195` `.expect("audio hub initialized")` | the `Option<HubInner>` is never `None` in practice; the `Option` is pure panic surface |
| `audio/mod.rs:181` `.expect("running set above")` | genuinely unreachable given the line above; still a panic in a hot-ish path |
| `audio/protocol.rs:7-10` `debug_assert!` | release builds skip it; the subsequent `copy_from_slice` would panic on a short buffer instead |

The parsers are clean: every fixed-offset read goes through `b.get(..)?`, and the direct
indexes (`b[1]`, `b[14]`, `b[38]`) are all preceded by an explicit length check. I found no
reachable index panic in `protocol.rs`. `parse_drop`'s pointer arithmetic uses `checked_add`
for the variable-length jumps; the `pos += 2/4/8` steps are unchecked but are always followed
by a bounds-checked read.

`windows.rs` uses raw pointer writes (`write_unaligned`, `copy_nonoverlapping`,
`windows.rs:678-679`) inside `unsafe`, guarded by a null check on `GlobalLock` — correct.
`bytemuck_u16` (`windows.rs:923-925`) does an unchecked `from_raw_parts` reinterpret of
`&[u16]` as `&[u8]`; alignment is fine (u16→u8 is a widening of alignment requirements
downward) and the lifetime is tied to the input, so it is sound, but it is hand-rolled where
`bytemuck` itself would be checked.

---

## 9. Silent fallbacks and swallowed errors

1. `mod.rs:70` — fast-queue overflow drops the event; only an atomic nobody reads.
2. `mod.rs:74` — `let _ = self.reliable.send(ev)` discards send failure.
3. `mod.rs:86` — `release_all` discards its send result.
4. `webrtc_session.rs:429-431` — `parse` returning `None` → bare `return`, no log, no counter.
5. `webrtc_session.rs:435` — `let _ = dc_pong.send(...)` discards the Pong result, undermining the only latency instrument.
6. `windows.rs:71` — `let _ = SetThreadPriority(...)`; a failed priority boost is invisible.
7. `windows.rs:77` — `timeBeginPeriod(1)` return value discarded (and the function is dead anyway).
8. `windows.rs:140-147` — synthetic device creation failure warns **once** then permanently degrades pen/touch → mouse. This is a *good* fallback (it logs and is deliberate) but the degradation is invisible thereafter and never re-probed.
9. `windows.rs:603-609` — if `set_clipboard` returns false, **nothing is logged at all**; the clipboard silently does not update. `set_clipboard` itself (`:866-880`) discards `EmptyClipboard` and `CloseClipboard` results and collapses three distinct failures into one `false`.
10. `windows.rs:666,671,680,686` — `GlobalFree`, `ScreenToClient`, `GlobalUnlock` results all discarded.
11. `windows.rs:851-853` — a partial `SendInput` warns but does **not** retry the remainder; input is silently half-applied.
12. `windows.rs:409` / `macos.rs:569` — unmapped keys go to `log::debug!`, i.e. invisible in a release build.
13. `macos.rs:265-270` — Accessibility not granted → one warning, then **every** `CGEventPost` is a silent no-op forever (finding).
14. `macos.rs:481,596,613,617` — every `CGEventCreate*` null check is `return`/`continue` with no log.
15. `macos.rs:769,781` — `setString`/`setData` return values are bound but the failure is only propagated as a bare `false` to a caller that ignores it.
16. `linux.rs:30-33` — warns once, then drops every event silently.
17. `audio/mod.rs:160` — `let _ = bridge_tx.send(pkt)` discards broadcast send errors (only fails when there are zero receivers, so benign, but it is where a "nobody is listening" condition would be visible).
18. `audio/mod.rs:207` — `let _ = bridge.join()` discards a panicked bridge thread.
19. `audio/encoder.rs:100-102` — `ctl_get(GET_LOOKAHEAD).unwrap_or(0)` reports zero algorithmic delay if the query fails; that value is propagated to the client's A/V alignment.
20. `webrtc_session.rs:796,849` — `RecvError::Lagged(_)` binds and discards the skipped count on both audio paths.
21. `webrtc_session.rs:741` — `let _ = spawn_blocking(unsubscribe).await` discards a join error, so a panicking unsubscribe would leave the refcount wrong silently.

Category 8 deserves credit as the model the other cases should follow: probe, log the reason
once, degrade deliberately. Categories 13 and 9 are the same situation handled badly.

---

## 10. TODO / FIXME / HACK / XXX

Only one block in the entire territory, in `linux.rs:1-11`, verbatim:

```
//! Linux injection backend — STUB (PRD §5.7).
//!
//! TODO: open /dev/uinput via `input-linux` or `uinput` crate.
//!   - Mouse/keyboard: EV_KEY / EV_REL / EV_ABS on a virtual device.
//!   - Multi-touch: create a MT type-B device (ABS_MT_SLOT, ABS_MT_TRACKING_ID,
//!     ABS_MT_POSITION_X/Y, ABS_MT_PRESSURE) and emit one SYN_REPORT per frame —
//!     one syscall per contact update, one SYN to commit, matching the touch model.
//!   - Pen: ABS_PRESSURE + ABS_TILT_X/Y on a tablet-style device.
//!   - Prefer direct uinput over shelling out to xdotool/ydotool (per-call process
//!     spawn is milliseconds of pure overhead).
//!   - Wayland vs X11: uinput is kernel-level and works under both.
```

No `FIXME`, `HACK` or `XXX` anywhere in either directory.

---

## 11. Latency path

Both the input path and the audio path run through this territory end to end.

### Input path (client → host cursor), per event

| Stage | Where | Estimated cost | Notes |
|---|---|---|---|
| SCTP receive + demux | webrtc-rs | — | out of territory |
| `on_message` closure entry | `webrtc_session.rs:425-428` | ~100 ns | 3 `Arc` clones + one `Box::pin` heap allocation **per message** |
| `parse` | `protocol.rs:235` | <1 µs | one `Vec` allocation for a batch |
| `route` → channel | `mod.rs:63-76` | ~100 ns | `try_send`; **or 0 and the event is gone** |
| **channel wait + thread wakeup** | `mod.rs:103-112` | **~10–60 µs typical, unbounded under load** | The single biggest serialization point on the input path. `select!` is random, not prioritized. |
| `refresh_geometry` | `windows.rs:196` / `macos.rs:325` | ~50 ns fresh; **~5 GetSystemMetrics + a monitor_rect lookup every 500 ms** | called once **per sample**, not per batch |
| OS injection | `windows.rs:850` / `macos.rs:494` | `SendInput` ~1–5 µs; `CGEventPost` to `kCGHIDEventTap` ~20–100 µs (Mach IPC to WindowServer) | **×N samples for a batch** — see the batch finding |
| WindowServer / win32k → app | OS | 1 frame | not ours |

The dominant avoidable costs are (a) the per-sample OS call for batched moves, (b) the
thread hop, and (c) the possibility of a 2.5-second replayed backlog when the fast queue
fills.

**Serialization points:** the single injector thread is a hard serializer for all input from
one client; the random `select!` means reliable and fast events cannot be prioritized relative
to each other; and the Pong reply awaits an SCTP send *inside* the receive handler
(`webrtc_session.rs:435`), head-of-line blocking the reliable channel.

### Audio path (host speaker mix → client), per 5 ms packet

| Stage | Where | Estimated cost |
|---|---|---|
| WASAPI/CoreAudio callback → PCM | out of territory | — |
| Opus encode | `encoder.rs:142` | ~0.1–0.3 ms (complexity 5, CELT-only) |
| Algorithmic lookahead | `encoder.rs:100` | 2.5 ms (120 samples), reported to the client |
| capture thread → crossbeam | out of territory | ~0 |
| **crossbeam → bridge thread wakeup** | `audio/mod.rs:158-161` | **~10–60 µs, unbounded under load, no priority boost** — a removable hop |
| bridge → broadcast → forwarder task wakeup | `audio/mod.rs:160` → `webrtc_session.rs:772` | tokio wakeup, ~10–50 µs |
| header write + double copy | `webrtc_session.rs:784-792` | <1 µs, one allocation |
| `buffered_amount().await` | `webrtc_session.rs:775` | every 4th packet |
| SCTP send | webrtc-rs | — |

Plus the frame duration itself (5 ms) and whatever the client's jitter buffer holds. The
structural point is that **there are three thread hops between capture and socket, and one of
them (the bridge) exists only as a type adapter and runs at default priority.**

### Shared timebase (asked for specifically)

`HOST_EPOCH: OnceLock<Instant>` (`audio/mod.rs:17`) is initialized by whichever caller runs
first. `Instant` is monotonic on every supported platform, so `host_now_ns()` is
**genuinely monotonic**, and `host_instant_to_ns` uses `saturating_duration_since`, so a
pre-epoch `Instant` clamps to 0 rather than wrapping.

It is **genuinely shared**: audio capture stamps `capture_ns` with `host_now_ns()`
(`windows_utils/audio/loopback.rs:245`, `macos_utils/audio/mod.rs:284`) and the video RTP
writer stamps with `host_ns_to_rtp90k(host_instant_to_ns(frame.capture))`
(`webrtc_session.rs:511-512`). Same `OnceLock`, same epoch, one clock. This part of the
design is correct and is the foundation the client's A/V sync rests on.

Two caveats: (1) `host_ns_to_rtp90k` truncates to u32 and wraps every ~13.25 h, which is
normal for RTP but means any host-side code comparing raw RTP timestamps must handle wrap;
(2) **the Opus-track fallback path bypasses the clock entirely** (`forward_track` discards
`capture_ns` and lets `write_sample` accumulate a fixed 5 ms), so on that path the shared
timebase guarantee does not hold — filed as a major finding.

---

## 12. Stale references

- `linux.rs:1` and `linux.rs:31` — "PRD §5.7". The only PRD in the tree is
  `linux-vdd/PRD.md`, a `vdutil` virtual-display document with no §5.7 and nothing about
  input. The second reference is inside a **user-visible log line**. Filed.
- `audio/protocol.rs` and `audio/mod.rs` contain no stale references; the CLAUDE.md-listed
  missing documents (`AUDIO_NOTES*`, `PRD-macos-legacy-audio.md`) are not cited from this
  territory.
- `docs/airplay-fallback-design.md` **does exist** — CLAUDE.md's caveat list should be read as
  covering the `AUDIO_NOTES*` family only. Not a defect here, but worth correcting upstream.
- No references to AI tooling anywhere in the territory.
- `macos.rs:172` `// Modifiers (REMAPPED)` accurately describes the code but not its
  consequence (it is unconditional, including for Apple clients) — a comment that is true and
  still misleading.
- `windows.rs:628` writes to a temp directory named `input-bridge-drop`, which appears to be a
  leftover from a previous project name ("input-bridge"); the crate is `screenextend`.

## 13. Dead code candidates

| Item | Evidence |
|---|---|
| `input::tune_process()` and all four backend impls | `grep -rn "tune_process" src-tauri/src/` returns **only** the five definitions. No caller. Means `timeBeginPeriod(1)` and `HIGH_PRIORITY_CLASS` never execute. Filed. |
| `input::boost_current_thread()` | No callers; the injector calls `backend::boost_thread()` directly (`mod.rs:99`). |
| `protocol::build_stats()` | No callers. |
| `InjectorTx::fast_queue_depth()` | No callers. |
| `Stats` / `fast_received` / `fast_dropped` | Written at `mod.rs:67,70`; **never read**. The `Arc<Stats>` is cloned into every `InjectorTx` for nothing. |
| `op::STATS` (0x62) | Never produced (`build_stats` dead) and never parsed (no arm in `parse`). |
| `InputEvent::Drag` | Parsed (`protocol.rs:395-404`) then explicitly ignored by **both** backends (`windows.rs:237`, `macos.rs:359`). |
| `InputEvent::Pong` | Parsed, ignored by both backends (`windows.rs:239`, `macos.rs:361`). The host only ever *sends* Pong. |
| `audio::protocol::parse()` and `AudioHeader` accessors | No production caller — the client decodes the header in `audio-worker.js`. Used only by the module's own tests. Keep (it is the executable spec for the format), but it is not live code. |
| `audio::protocol::build_message()` | Test-only. |
| `OpusEncoder::version()` | No callers; `OpusApi::version()` exists solely to feed it. |
| `OpusApi::encoder_get_size` and `encode` (the i16 variant) | Resolved at load (`opus_sys.rs:139-146`) but never invoked; only `encode_float` is used. A missing symbol would fail `load()` for no reason. |
| `opus_sys` constants `OPUS_SET_APPLICATION/MAX_BANDWIDTH/BANDWIDTH/PACKET_LOSS_PERC/FORCE_CHANNELS/LSB_DEPTH/RESET_STATE/GET_SAMPLE_RATE`, `OPUS_AUTO`, `OPUS_BITRATE_MAX`, `OPUS_APPLICATION_VOIP/AUDIO`, `OPUS_SIGNAL_VOICE` | Declared, never used. Harmless as documentation of the ABI; `PACKET_LOSS_PERC` in particular should become live (see the FEC finding). |
| `generic.rs` | `cfg(not(any(windows, macos, linux)))` — no such target is built. |
| `SeqGate` (`audio/tests.rs:74-115`) | Test-only model of client behaviour; no production analogue exists. Worth noting because the *input* protocol has no sequence numbers at all and could use one. |

`protocol.rs:1` `#![allow(dead_code)]` is what keeps all of the above from producing warnings.
Narrowing that allow would surface most of this list automatically.

## 14. Capability probing

**Done correctly:** `windows.rs:140-147`. `CreateSyntheticPointerDevice` is *attempted*, and
failure degrades pen/touch to mouse emulation with a logged reason. No version check, no
`GetVersionEx` — the capability is probed by trying to use it. This is the tier doctrine
applied properly and is the model for the rest.

**Missing entirely — macOS Accessibility.** `macos.rs:265-270` checks
`accessibility_trusted()`, logs a warning, and then **proceeds regardless**. There is no tier
enum, nothing in `CompatibilityReport` (which has `audio_backend` and `display_backend` but no
input equivalent), and no re-probe — the warning even tells the user to relaunch, because the
check happens once in the constructor. The failure mode is a completely silent no-op. Filed as
a major finding. This is the single clearest tier gap in the territory.

**Missing — Linux.** `linux.rs` is a stub that reports nothing to the UI; a Linux host would
present remote control as available and silently drop everything. Consistent with "Linux is
scaffolded", but there is no tier saying so.

**No version-number gating found.** Neither backend branches on an OS version number. macOS
uses only floor-present CoreGraphics symbols; Windows uses only APIs present since Windows 8.
On this axis the territory is clean — the defect is the *absence* of a probe on macOS, not the
presence of a version check.

**Audio.** `audio/` itself does no probing (correctly — `platform::start_audio_capture()`
owns the tiering, per CLAUDE.md), but note that `AudioHub::subscribe` treats a failed capture
start as a per-subscription error (`webrtc_session.rs:731-734`, "audio: subscribe failed")
rather than consulting the already-computed backend tier. A user on an unsupported macOS
version gets a per-session error string instead of the `NeedsDriverInstall` tier the UI
already knows how to render.

## 15. Questions and cross-cutting concerns

1. **Cross-territory (D/E — audio backends): does every `AudioStopFn` guarantee the crossbeam
   `Sender` is dropped before it returns?** If not, `bridge.join()` at `audio/mod.rs:207`
   hangs **while holding the hub mutex**, deadlocking the entire audio subsystem. Please have
   the Windows-audio and macOS-audio agents confirm. This is the highest-severity open
   question I have.
2. **Cross-territory (WebRTC): is the `RTCPeerConnection` ever actually dropped?** Another
   agent filed `major-webrtc-peerconnection-never-closed-arc-cycle.md`. If it leaks, the
   `InjectorTx` clones held in the DataChannel closures leak with it, so the per-session
   `THREAD_PRIORITY_TIME_CRITICAL` injector thread and its two synthetic HID device handles
   never go away. The handle leak I filed and that Arc cycle compound.
3. **Cross-territory (macOS): what QoS class does
   `macos_utils::streamer::qos::pin_current_thread_user_interactive()` actually set, and does
   it check the return value?** Another agent filed
   `major-vt-qos-thread-policy-returns-ignored.md`; if the same helper is used, the injector
   thread's boost may be silently failing too.
4. **Cross-territory (audio backends): what is the depth and blocking behaviour of the
   crossbeam channel inside `AudioCapture`?** I could not see it from here, and it is the one
   queue on the audio path whose back-pressure semantics I could not characterize.
5. **Orchestrator decision: is the relative-pointer escape from the target display intended?**
   A client under pointer lock can drive the host's real monitors (unclamped on Windows,
   desktop-clamped on macOS) while the absolute path is confined to the virtual display. This
   needs a product answer before it gets a technical one.
6. **Orchestrator decision: should the injector be per-session or per-host?** There is one
   physical cursor. Today N clients get N threads, N device pairs, and no arbitration — two
   clients can fight over the cursor silently. Consolidating fixes the leak, the priority
   pile-up and the arbitration gap at once, but it is a real refactor.
7. **Should the input protocol get sequence numbers?** The audio protocol has them and the
   client has a `SeqGate`-shaped policy; input has neither, which is why the fast/reliable
   split can misplace a click. Adding a seq to the fast channel would let the injector reject
   stale moves cheaply.
8. **Note for whoever owns compatibility:** the client-page territory should confirm what the
   client does with `lookahead_samples`, because `encoder.rs:100`'s `.unwrap_or(0)` can feed
   it a false zero.
