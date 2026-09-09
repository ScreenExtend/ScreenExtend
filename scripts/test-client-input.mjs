// Golden vectors for the input wire protocol, produced by the shipped `input.js` and parsed by
// the shipped Rust parser.
//
// `streamer/input/protocol.rs` has tests for the format and `input.js` writes the same format
// with no test — the sharpest version of the client/host coverage gap, because both sides can be
// individually correct about different layouts and nothing notices. This closes it in the only
// way that does not restate one side in the other's language:
//
//   1. This harness drives `input.js` under Node and asserts every field of every frame it
//      emits, at the offsets `docs/INPUT-PROTOCOL.md` documents.
//   2. It writes those exact bytes to `src-tauri/src/streamer/input/client-frames.json`
//      (with `--write`), which is checked in.
//   3. `streamer/input/protocol.rs`'s `client_golden_vectors_still_parse` feeds each one to the
//      real `parse()` and asserts the decoded event.
//
// So a client-side change to the bytes fails here, and a host-side change to the parser fails
// there, against the same fixture. Neither side can drift alone.
//
//   node scripts/test-client-input.mjs [path/to/static] [--write]
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2).filter((a) => a !== '--write');
const WRITE = process.argv.includes('--write');
const STATIC = args[0] || 'src-tauri/src/streamer/static';
// Next to the parser that reads it: `streamer/input/client-frames.json`, `include_str!`'d by
// `protocol.rs` the way `streamer/test/trickle-fragment.sdpfrag` is.
const FIXTURE = path.join(path.dirname(STATIC), 'input', 'client-frames.json');

let failures = 0;
const ok = (m) => console.log('ok  - ' + m);
const fail = (m) => { failures++; console.log('FAIL- ' + m); };
const eq = (name, got, want) => (got === want ? ok(name + ' = ' + got) : fail(name + ' = ' + got + ', expected ' + want));
const close = (name, got, want) => (Math.abs(got - want) < 1e-4 ? ok(name + ' ~= ' + got) : fail(name + ' = ' + got + ', expected ' + want));

// ── input.js under the smallest browser it will run in ───────────────────────────────────
const src = fs.readFileSync(STATIC + '/input.js', 'utf8');
const sent = [];
const chan = (label) => ({
  label,
  readyState: 'open',
  bufferedAmount: 0,
  binaryType: '',
  onmessage: null, onopen: null, onclose: null,
  send(buf) {
    sent.push({
      label,
      bytes: new Uint8Array(buf.buffer || buf, buf.byteOffset || 0, buf.byteLength || buf.length),
    });
  },
  close() {},
});
const channels = {};
const listeners = [];
const el = () => ({
  clientWidth: 1280, clientHeight: 720, style: {}, value: '',
  addEventListener: (...a) => listeners.push(a), removeEventListener() {},
  focus() {}, blur() {},
});

let now = 1000;
const ctx = {
  console: { log() {}, warn() {}, error() {} },
  performance: { now: () => now, timeOrigin: 1_700_000_000_000 },
  setInterval: (fn) => { ctx.__tick = fn; return 1; },
  clearInterval() {},
  setTimeout: () => 0,
  clearTimeout() {},
  TextEncoder,
  MessageChannel: class { constructor() { this.port1 = { postMessage() {} }; this.port2 = {}; } },
  PointerEvent: class { getCoalescedEvents() { return []; } },
  navigator: { maxTouchPoints: 0, keyboard: { lock() {} } },
  document: {
    addEventListener: (...a) => listeners.push(a),
    removeEventListener() {},
    pointerLockElement: null,
    visibilityState: 'visible',
    getElementById: () => el(),
  },
  localStorage: { getItem: () => null, setItem() {} },
};
ctx.window = ctx;
ctx.self = ctx;
ctx.window.innerWidth = 1280;
ctx.window.innerHeight = 720;
ctx.window.devicePixelRatio = 2;
ctx.window.addEventListener = (...a) => listeners.push(a);
vm.createContext(ctx);
vm.runInContext(src, ctx, { filename: 'input.js' });

ctx.SEVideo = { getStats: () => ({}) };
ctx.RemoteInput.setup({ createDataChannel(label) { channels[label] = chan(label); return channels[label]; } });
if (channels.reliable && channels.reliable.onopen) channels.reliable.onopen();

const fire = (type, ev) => {
  const hit = listeners.filter((l) => l[0] === type);
  if (!hit.length) { fail('no listener registered for ' + type); return; }
  hit.forEach((l) => l[1](ev));
};
const lastOf = (op) => { for (let i = sent.length - 1; i >= 0; i--) if (sent[i].bytes[0] === op) return sent[i]; return null; };
const view = (f) => new DataView(f.bytes.buffer, f.bytes.byteOffset, f.bytes.byteLength);
const hex = (bytes) => Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');

const OP = {
  POINTER_DOWN: 0x01, POINTER_UP: 0x02, POINTER_MOVE: 0x03, POINTER_CANCEL: 0x04,
  POINTER_ENTER: 0x05, POINTER_LEAVE: 0x06, POINTER_OVER: 0x07, POINTER_OUT: 0x08,
  POINTER_MOVE_BATCH: 0x09, WHEEL: 0x10, ZOOM: 0x11, KEY: 0x20, TEXT_INPUT: 0x21,
  CLIPBOARD: 0x30, FOCUS_STATE: 0x50, VISIBILITY: 0x51, RESIZE: 0x52,
  POINTERLOCK_STATE: 0x53, MOUSE_DELTA: 0x54, PING: 0x60,
};

// Everything the fixture will carry: a label the Rust side asserts on, and the bytes.
const vectors = [];
const capture = (name, frame) => {
  if (!frame) { fail('no frame captured for ' + name); return null; }
  vectors.push({ name, channel: frame.label, hex: hex(frame.bytes) });
  return frame;
};

// ── POINTER_DOWN: the 40-byte fixed layout ───────────────────────────────────────────────
// op u8 @0, source u8 @1, pointerId u32 @2, x f32 @6, y f32 @10, pressure f32 @14,
// tiltX f32 @18, tiltY f32 @22, twist f32 @26, w f32 @30, h f32 @34, buttons u16 @38.
sent.length = 0;
fire('pointerdown', {
  pointerId: 7, pointerType: 'pen', clientX: 640, clientY: 360,
  pressure: 0.5, tiltX: -20, tiltY: 30, twist: 90, width: 3, height: 4, buttons: 1,
  preventDefault() {}, getCoalescedEvents: undefined,
});
const down = capture('pointer_down_pen', lastOf(OP.POINTER_DOWN));
if (down) {
  const d = view(down);
  eq('POINTER_DOWN length', down.bytes.length, 40);
  eq('POINTER_DOWN channel', down.label, 'reliable');
  eq('POINTER_DOWN source (2 = pen)', d.getUint8(1), 2);
  eq('POINTER_DOWN pointer id', d.getUint32(2, true), 7);
  close('POINTER_DOWN pressure', d.getFloat32(14, true), 0.5);
  close('POINTER_DOWN tiltX', d.getFloat32(18, true), -20);
  close('POINTER_DOWN tiltY', d.getFloat32(22, true), 30);
  close('POINTER_DOWN twist', d.getFloat32(26, true), 90);
  eq('POINTER_DOWN buttons', d.getUint16(38, true), 1);
  // x and y are normalised against the video rect, so they are in 0..1 rather than CSS pixels.
  const x = d.getFloat32(6, true);
  const y = d.getFloat32(10, true);
  if (x >= 0 && x <= 1 && y >= 0 && y <= 1) ok('POINTER_DOWN x/y are normalised into 0..1');
  else fail('POINTER_DOWN x=' + x + ' y=' + y + ' is not normalised');
}

// ── POINTER_UP and POINTER_CANCEL share the layout, and the opcode is the phase ──────────
sent.length = 0;
fire('pointerup', {
  pointerId: 7, pointerType: 'pen', clientX: 640, clientY: 360, pressure: 0,
  tiltX: 0, tiltY: 0, twist: 0, width: 1, height: 1, buttons: 0,
  preventDefault() {}, getCoalescedEvents: undefined,
});
const up = capture('pointer_up_pen', lastOf(OP.POINTER_UP));
if (up) eq('POINTER_UP length', up.bytes.length, 40);

sent.length = 0;
fire('pointercancel', {
  pointerId: 7, pointerType: 'touch', clientX: 100, clientY: 100, pressure: 0,
  tiltX: 0, tiltY: 0, twist: 0, width: 1, height: 1, buttons: 0,
  preventDefault() {}, getCoalescedEvents: undefined,
});
const cancel = capture('pointer_cancel_touch', lastOf(OP.POINTER_CANCEL));
if (cancel) eq('POINTER_CANCEL length', cancel.bytes.length, 40);

// ── WHEEL: 15 bytes — op, source, dx f32, dy f32, dz f32, mode u8 ────────────────────────
sent.length = 0;
fire('wheel', { deltaX: -12.5, deltaY: 120, deltaZ: 0, deltaMode: 1, preventDefault() {} });
const wheel = capture('wheel_lines', lastOf(OP.WHEEL));
if (wheel) {
  const d = view(wheel);
  eq('WHEEL length', wheel.bytes.length, 15);
  close('WHEEL dx', d.getFloat32(2, true), -12.5);
  close('WHEEL dy', d.getFloat32(6, true), 120);
  close('WHEEL dz', d.getFloat32(10, true), 0);
  eq('WHEEL deltaMode', d.getUint8(14), 1);
}

// ── KEY: op, down u8, repeat u8, mods u16, then two length-prefixed strings ──────────────
sent.length = 0;
fire('keydown', {
  code: 'KeyA', key: 'a', repeat: false,
  shiftKey: true, ctrlKey: false, altKey: false, metaKey: false,
  preventDefault() {}, getModifierState: () => false,
});
const key = capture('key_down_shift_a', lastOf(OP.KEY));
if (key) {
  // op @0, flags u8 @1 (bit0 down, bit1 repeat), mods u16 @2, len(code) u8 @4, code @5.
  const d = view(key);
  eq('KEY down bit', d.getUint8(1) & 1, 1);
  eq('KEY repeat bit', (d.getUint8(1) >> 1) & 1, 0);
  if (d.getUint16(2, true) !== 0) ok('KEY carries a modifier bitfield = ' + d.getUint16(2, true));
  else fail('shiftKey did not reach the modifier bitfield');
  const codeLen = d.getUint8(4);
  const code = new TextDecoder().decode(key.bytes.subarray(5, 5 + codeLen));
  eq('KEY code string', code, 'KeyA');
  const keyLen = d.getUint8(5 + codeLen);
  eq('KEY key string', new TextDecoder().decode(key.bytes.subarray(6 + codeLen, 6 + codeLen + keyLen)), 'a');
}

sent.length = 0;
fire('keyup', {
  code: 'KeyA', key: 'a', repeat: false,
  shiftKey: false, ctrlKey: false, altKey: false, metaKey: false,
  preventDefault() {}, getModifierState: () => false,
});
capture('key_up_a', lastOf(OP.KEY));

// ── PING: op + u64 of client monotonic nanoseconds ───────────────────────────────────────
sent.length = 0;
now = 12_345;
if (typeof ctx.__tick === 'function') ctx.__tick();
const ping = capture('ping', lastOf(OP.PING));
if (ping) {
  const d = view(ping);
  eq('PING length', ping.bytes.length, 9);
  const t = d.getUint32(1, true) + d.getUint32(5, true) * 4294967296;
  eq('PING stamp is performance.now() in ns', t, Math.round(12_345 * 1e6));
}

// ── The lifecycle frames: two bytes each ─────────────────────────────────────────────────
sent.length = 0;
fire('blur', {});
const focus = lastOf(OP.FOCUS_STATE);
if (focus) {
  capture('focus_lost', focus);
  eq('FOCUS_STATE length', focus.bytes.length, 2);
  eq('FOCUS_STATE payload', focus.bytes[1], 0);
} else fail('blur produced no FOCUS_STATE');

// ── MOUSE_DELTA under pointer lock: op, dx i16, dy i16, buttons u16, flags u8 ────────────
ctx.document.pointerLockElement = {};
sent.length = 0;
now = 20_000;
const moveType = listeners.some((l) => l[0] === 'pointerrawupdate') ? 'pointerrawupdate' : 'pointermove';
fire(moveType, {
  pointerId: 1, pointerType: 'mouse', clientX: 640, clientY: 360,
  movementX: -5, movementY: 3, pressure: 0, tiltX: 0, tiltY: 0, twist: 0,
  width: 1, height: 1, buttons: 2, preventDefault() {}, getCoalescedEvents: undefined,
});
const delta = capture('mouse_delta_locked', lastOf(OP.MOUSE_DELTA));
if (delta) {
  // op @0, source @1, dx i16 @2, dy i16 @4, buttons u16 @6, flags u8 @8.
  const d = view(delta);
  eq('MOUSE_DELTA dx', d.getInt16(2, true), -5);
  eq('MOUSE_DELTA dy', d.getInt16(4, true), 3);
  eq('MOUSE_DELTA buttons', d.getUint16(6, true), 2);
  eq('MOUSE_DELTA flags byte (no unadjusted grant)', d.getUint8(8), 0);
}
ctx.document.pointerLockElement = null;

// ── Fixture ──────────────────────────────────────────────────────────────────────────────
if (!vectors.length) fail('no vectors were captured');

const payload = {
  readme: [
    "Golden vectors for the client -> host input protocol. GENERATED, do not hand-edit.",
    "",
    "Produced by `node scripts/test-client-input.mjs --write` from the shipped input.js, and",
    "parsed by `streamer::input::protocol`'s `client_golden_vectors_still_parse`. A change to",
    "the bytes on either side fails against this file rather than silently diverging.",
    "",
    "`hex` is the whole DataChannel message. `channel` is which of the three it went out on.",
    "docs/INPUT-PROTOCOL.md is the prose.",
  ],
  vectors,
};
const serialized = JSON.stringify(payload, null, 2) + '\n';

if (WRITE) {
  fs.mkdirSync(path.dirname(FIXTURE), { recursive: true });
  fs.writeFileSync(FIXTURE, serialized);
  console.log('wrote ' + FIXTURE + ' (' + vectors.length + ' vectors)');
} else if (fs.existsSync(FIXTURE)) {
  const onDisk = fs.readFileSync(FIXTURE, 'utf8');
  const want = JSON.parse(onDisk).vectors;
  const byName = new Map(want.map((v) => [v.name, v]));
  let drifted = 0;
  for (const v of vectors) {
    const w = byName.get(v.name);
    if (!w) { fail('vector ' + v.name + ' is new; re-run with --write'); drifted++; continue; }
    if (w.hex !== v.hex) {
      fail('vector ' + v.name + ' changed:\n    was ' + w.hex + '\n    now ' + v.hex);
      drifted++;
    }
    if (w.channel !== v.channel) { fail('vector ' + v.name + ' moved to the ' + v.channel + ' channel'); drifted++; }
  }
  for (const w of want) {
    if (!vectors.some((v) => v.name === w.name)) fail('vector ' + w.name + ' is no longer produced');
  }
  if (!drifted) ok(vectors.length + ' golden vectors are byte-identical to the checked-in fixture');
} else {
  fail('no fixture at ' + FIXTURE + '; run with --write');
}

if (failures) {
  console.log('\n' + failures + ' input-protocol assertion(s) failed');
  process.exitCode = 1;
} else {
  console.log('\nall client input-protocol assertions passed');
}
