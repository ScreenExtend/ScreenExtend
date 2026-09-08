// Runs input.js under Node with the smallest possible browser stubs, drives one PING/PONG
// exchange and one STATS frame, and asserts the wire bytes against the layout the Rust parser
// expects. Not a substitute for a browser, but it catches the class of bug that would silently
// kill the once-a-second timer: a TypeError inside sendStats().
import fs from 'node:fs';
import vm from 'node:vm';

const STATIC = process.argv[2] || 'src-tauri/src/streamer/static';
const src = fs.readFileSync(STATIC + '/input.js', 'utf8');

const sent = [];
const chan = (label) => ({
  label,
  readyState: 'open',
  bufferedAmount: 0,
  binaryType: '',
  onmessage: null,
  onopen: null,
  onclose: null,
  send(buf) { sent.push({ label, bytes: new Uint8Array(buf.buffer || buf, buf.byteOffset || 0, buf.byteLength || buf.length) }); },
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
  console,
  performance: { now: () => now, timeOrigin: 1_700_000_000_000 },
  setInterval: (fn) => { ctx.__tick = fn; return 1; },
  clearInterval() {},
  setTimeout: () => 0,
  clearTimeout() {},
  TextEncoder,
  MessageChannel: class { constructor() { this.port1 = { postMessage() {} }; this.port2 = {}; } },
  // input.js decides at load whether to read coalesced samples from the prototype, so the
  // stub has to exist before the module runs or the batch path is unreachable here.
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

const OP = { PING: 0x60, PONG: 0x61, STATS: 0x62 };
const fail = (m) => { console.error('FAIL: ' + m); process.exitCode = 1; };
const ok = (m) => console.log('ok  - ' + m);

if (typeof ctx.SEClock !== 'object') fail('SEClock was not exported');
if (ctx.SEClock.offsetMs !== null) fail('SEClock starts with an offset it never measured');
if (ctx.SEClock.hostNsToAbsMs(1e9) !== null) fail('hostNsToAbsMs must refuse to guess without an offset');
ok('SEClock reports no offset before the first PONG');

// Wire up the channels the way index.html does.
const pc = {
  createDataChannel(label) { channels[label] = chan(label); return channels[label]; },
};
ctx.SEVideo = {
  getStats: () => ({
    videoPath: 1, audioPath: 2,
    sab: true, crossOriginIsolated: true, wakeLock: false, pointerLock: false,
    keyboardLock: true, fullscreen: true, setSinkId: false, decoderHardware: true,
    clockValid: true, rttMs: 4.1,
    arrivalToPaint: { p50: 3.5, p90: 6.25, p99: 11.0 },
    decode: { p50: 2.0, p90: 3.0, p99: 4.0 },
    hostToPaint: { p50: 41.2, p90: 55.0, p99: 90.0 },
    framesPainted: 1799, framesDropped: 3, decodeErrors: 0, keyRequests: 2,
    audioUnderruns: 7, audioOverruns: 1,
    transportTier: 1, renderTier: 1, keyTier: 1, sinkTier: 2,
    suppressed: 9, degraded: (1 << 3) | (1 << 13),
    audioTargetMs: 18.5, audioConcealed: 4, audioOutputLatencyMs: 12.25,
    audioRedundantUsed: 17, audioRedundantDiscarded: 265,
  }),
};

ctx.RemoteInput.setup(pc);
channels.reliable.onopen();
if (typeof ctx.__tick !== 'function') fail('no 1 s timer was armed on the reliable channel');

sent.length = 0;
ctx.__tick();

const ping = sent.find((s) => s.bytes[0] === OP.PING);
if (!ping) fail('no PING was sent');
else if (ping.bytes.length !== 9) fail('PING is ' + ping.bytes.length + ' bytes, expected 9');
else ok('PING is 9 bytes on the reliable channel');

const stats = sent.find((s) => s.bytes[0] === OP.STATS);
if (!stats) { fail('no STATS frame was sent'); process.exit(); }
const b = stats.bytes;
const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
if (b.length !== 104) fail('STATS is ' + b.length + ' bytes, expected the 104 of v3');
else ok('STATS is exactly the 104 bytes docs/TELEMETRY.md documents for v3');
if (b.length < 72) fail('STATS is shorter than the 72-byte floor the Rust parser requires');
else ok('STATS clears the 72-byte floor an older host reads');
if (b.length < 96) fail('STATS is shorter than the 96 bytes a v2 host reads the tail from');
else ok('STATS clears the 96-byte v2 tail a v2 host reads');
if (stats.label !== 'reliable') fail('STATS went out on ' + stats.label + ', not reliable');
else ok('STATS goes on the reliable channel');

const eq = (name, got, want) => (got === want ? ok(name + ' = ' + got) : fail(name + ' = ' + got + ', expected ' + want));
eq('version', dv.getUint8(1), 3);
eq('video path', dv.getUint8(2), 1);
eq('audio path', dv.getUint8(3), 2);
// SAB | ISOLATED | KEYBOARD_LOCK | FULLSCREEN | DECODER_HW | CLOCK_VALID
eq('flags', dv.getUint32(4, true), (1 << 0) | (1 << 1) | (1 << 4) | (1 << 5) | (1 << 7) | (1 << 8));
eq('arrival->paint p50 us', dv.getUint32(8, true), 3500);
eq('arrival->paint p90 us', dv.getUint32(12, true), 6250);
eq('decode p50 us', dv.getUint32(20, true), 2000);
eq('host->paint p50 us', dv.getUint32(32, true), 41200);
eq('frames painted', dv.getUint32(44, true), 1799);
eq('frames dropped', dv.getUint32(48, true), 3);
eq('keyframe requests', dv.getUint32(56, true), 2);
eq('audio underruns', dv.getUint32(60, true), 7);
eq('audio overruns', dv.getUint32(64, true), 1);
eq('rtt us', dv.getUint32(68, true), 4100);
// v2 tail. The first 72 bytes above are byte-identical to v1, which is what lets an older
// host read this frame's prefix and ignore the rest.
eq('transport tier', dv.getUint8(72), 1);
eq('render tier', dv.getUint8(73), 1);
eq('keyframe tier', dv.getUint8(74), 1);
eq('sink tier', dv.getUint8(75), 2);
eq('suppressed errors', dv.getUint32(76, true), 9);
eq('degradation flags', dv.getUint32(80, true), (1 << 3) | (1 << 13));
eq('audio target us', dv.getUint32(84, true), 18500);
eq('audio concealed', dv.getUint32(88, true), 4);
eq('audio output latency us', dv.getUint32(92, true), 12250);
// v3 tail. Bytes 0..96 are byte-identical to v2, so a v2 host reads this frame's prefix and
// ignores the rest.
eq('audio redundant used', dv.getUint32(96, true), 17);
eq('audio redundant discarded', dv.getUint32(100, true), 265);

// A host PONG carrying its clock: t0 = 1000 ms in ns, host at 5000 ms, reply read at 1010 ms.
now = 1010;
const pong = new ArrayBuffer(17);
const pv = new DataView(pong);
pv.setUint8(0, OP.PONG);
pv.setBigUint64(1, BigInt(Math.round(1000 * 1e6)), true);
pv.setBigUint64(9, BigInt(Math.round(5000 * 1e6)), true);
channels.reliable.onmessage({ data: pong });

const expectOffset = 5000 - (ctx.performance.timeOrigin + 1005);
if (ctx.SEClock.offsetMs === null) fail('the PONG did not produce a clock offset');
else if (Math.abs(ctx.SEClock.offsetMs - expectOffset) > 1e-6) {
  fail('offset ' + ctx.SEClock.offsetMs + ', expected ' + expectOffset);
} else ok('clock offset is the NTP midpoint estimate');
if (Math.abs(ctx.SEClock.rttMs - 10) > 1e-6) fail('rtt ' + ctx.SEClock.rttMs + ', expected 10');
else ok('rtt is the round trip, 10 ms');

// A 9-byte PONG from an older host must give an RTT and no offset.
const before = ctx.SEClock.offsetMs;
const short = new ArrayBuffer(9);
const sv = new DataView(short);
sv.setUint8(0, OP.PONG);
sv.setBigUint64(1, BigInt(Math.round(1005 * 1e6)), true);
channels.reliable.onmessage({ data: short });
if (ctx.SEClock.offsetMs !== before) fail('a legacy 9-byte PONG moved the clock offset');
else ok('a legacy 9-byte PONG is tolerated and does not move the offset');

// Host capabilities ride the PONG tail. Nothing optional may be used until the host has said
// it understands it: a 17-byte PONG is an older host and means no bits.
const KEYFRAME_REQUEST_BIT = 1 << 0;
if (ctx.RemoteInput.hostCapabilities() !== 0) fail('capabilities were assumed before any host said so');
else ok('an older host (17-byte PONG) advertises no capabilities');
sent.length = 0;
if (ctx.RemoteInput.requestKeyframe() !== false) fail('a keyframe request was claimed against a host that drops the opcode');
else if (sent.some((s) => s.bytes[0] === 0x64)) fail('0x64 was sent to a host that does not parse it');
else ok('requestKeyframe() refuses, and sends nothing, without the host capability');

let capsSeen = null;
ctx.RemoteInput.onHostCapabilities((c) => { capsSeen = c; });
now = 1200;
const pong19 = new ArrayBuffer(19);
const p19 = new DataView(pong19);
p19.setUint8(0, OP.PONG);
p19.setBigUint64(1, BigInt(Math.round(1195 * 1e6)), true);
p19.setBigUint64(9, BigInt(Math.round(5200 * 1e6)), true);
p19.setUint16(17, KEYFRAME_REQUEST_BIT | (1 << 1), true);
channels.reliable.onmessage({ data: pong19 });

if (capsSeen !== (KEYFRAME_REQUEST_BIT | (1 << 1))) fail('capability subscribers saw ' + capsSeen);
else ok('the PONG capability bitfield reaches subscribers');
if (!ctx.RemoteInput.hostCapability('KEYFRAME_REQUEST')) fail('bit 0 did not enable the keyframe request');
else ok('bit 0 enables the out-of-band keyframe request');
if (!ctx.RemoteInput.hostCapability('STATS_V2')) fail('bit 1 was not decoded');
else ok('bit 1 decodes as STATS_V2');
if (ctx.RemoteInput.hostCapability('UNRELIABLE_MOVE_CHANNEL')) fail('an unset bit read as set');
else ok('an unset capability bit reads as unset');

sent.length = 0;
now = 1500;
if (ctx.RemoteInput.requestKeyframe() !== true) fail('the keyframe request was refused against a capable host');
else {
  const kf = sent.find((s) => s.bytes[0] === 0x64);
  if (!kf) fail('no 0x64 frame was sent');
  else if (kf.bytes.length !== 2) fail('KEYFRAME_REQUEST is ' + kf.bytes.length + ' bytes, expected 2');
  else if (kf.label !== 'reliable') fail('KEYFRAME_REQUEST went out on ' + kf.label);
  else ok('KEYFRAME_REQUEST is 2 bytes on the reliable channel once the host advertises it');
}

// The 200 ms limiter must only be armed by a request that actually went out.
sent.length = 0;
now = 1550;
if (ctx.RemoteInput.requestKeyframe() !== false) fail('the 200 ms keyframe limiter did not hold');
else ok('a second keyframe request inside 200 ms is suppressed');
channels.reliable.readyState = 'closed';
now = 5000;
if (ctx.RemoteInput.requestKeyframe() !== false) fail('a request was claimed on a closed channel');
else ok('a closed channel refuses the request');
channels.reliable.readyState = 'open';
now = 5001;
if (ctx.RemoteInput.requestKeyframe() !== true) fail('the attempt on a closed channel burned the rate-limit window');
else ok('an attempt that could not send does not burn the rate-limit window');


// ── The optional input tails (docs/INPUT-PROTOCOL.md) ────────────────────────────────────
// Both are gated on the host having advertised that it parses them. Until then every frame
// must be byte-identical to v1, because the tail bytes would otherwise be read as nothing at
// best and as the MOUSE_DELTA pad at worst.
const INPUT_TIMESTAMP_BIT = 1 << 3;
const INPUT_EVENT_FLAGS_BIT = 1 << 4;
const fire = (type, ev) => {
  const hit = listeners.filter((l) => l[0] === type);
  if (!hit.length) { fail('no listener registered for ' + type); return; }
  hit.forEach((l) => l[1](ev));
};
const pointerEvent = (over) => Object.assign({
  pointerId: 7, pointerType: 'mouse', clientX: 640, clientY: 360,
  pressure: 0.5, tiltX: 0, tiltY: 0, twist: 0, width: 1, height: 1, buttons: 1,
  preventDefault() {}, getCoalescedEvents: undefined,
}, over || {});
const wheelEvent = { deltaX: 0, deltaY: 120, deltaZ: 0, deltaMode: 0, preventDefault() {} };
const keyEvent = {
  code: 'KeyA', key: 'a', repeat: false, shiftKey: false, ctrlKey: false,
  altKey: false, metaKey: false, preventDefault() {}, getModifierState: () => false,
};
// The page registers pointerrawupdate where the engine has it and pointermove otherwise;
// under the stubs it is the latter, so drive whichever one is installed.
const firePointerMove = (ev) => fire(listeners.some((l) => l[0] === 'pointerrawupdate') ? 'pointerrawupdate' : 'pointermove', ev);
const lastOf = (op) => { for (let i = sent.length - 1; i >= 0; i--) if (sent[i].bytes[0] === op) return sent[i]; return null; };
const OPI = { POINTER_DOWN: 0x01, WHEEL: 0x10, KEY: 0x20, MOUSE_DELTA: 0x54 };

sent.length = 0;
fire('pointerdown', pointerEvent());
fire('wheel', wheelEvent);
fire('keydown', keyEvent);
const v1Pointer = lastOf(OPI.POINTER_DOWN), v1Wheel = lastOf(OPI.WHEEL), v1Key = lastOf(OPI.KEY);
if (!v1Pointer || v1Pointer.bytes.length !== 40) fail('POINTER_DOWN is ' + (v1Pointer && v1Pointer.bytes.length) + ' bytes against a host with no INPUT_TIMESTAMP');
else ok('POINTER_* stays 40 bytes until the host asks for a stamp');
if (!v1Wheel || v1Wheel.bytes.length !== 15) fail('WHEEL is ' + (v1Wheel && v1Wheel.bytes.length) + ' bytes against a host with no INPUT_TIMESTAMP');
else ok('WHEEL stays 15 bytes until the host asks for a stamp');
const v1KeyLen = v1Key && v1Key.bytes.length;

// Now the host says it reads both tails.
now = 6000;
const pongCaps = new ArrayBuffer(19);
const pc19 = new DataView(pongCaps);
pc19.setUint8(0, OP.PONG);
pc19.setBigUint64(1, BigInt(Math.round(5995 * 1e6)), true);
pc19.setBigUint64(9, BigInt(Math.round(9000 * 1e6)), true);
pc19.setUint16(17, KEYFRAME_REQUEST_BIT | INPUT_TIMESTAMP_BIT | INPUT_EVENT_FLAGS_BIT, true);
channels.reliable.onmessage({ data: pongCaps });
if (!ctx.RemoteInput.hostCapability('INPUT_TIMESTAMP')) fail('bit 3 did not decode as INPUT_TIMESTAMP');
else ok('bit 3 decodes as INPUT_TIMESTAMP');

now = 6100;
sent.length = 0;
fire('pointerdown', pointerEvent());
fire('wheel', wheelEvent);
fire('keydown', Object.assign({}, keyEvent, { code: 'KeyB', key: 'b' }));
const p2 = lastOf(OPI.POINTER_DOWN), w2 = lastOf(OPI.WHEEL), k2 = lastOf(OPI.KEY);
const stampAt = (s, off) => {
  const d = new DataView(s.bytes.buffer, s.bytes.byteOffset, s.bytes.byteLength);
  return d.getUint32(off, true) + d.getUint32(off + 4, true) * 4294967296;
};
if (!p2 || p2.bytes.length !== 48) fail('stamped POINTER_DOWN is ' + (p2 && p2.bytes.length) + ' bytes, expected 48');
else if (stampAt(p2, 40) !== Math.round(6100 * 1e6)) fail('POINTER_DOWN stamp is ' + stampAt(p2, 40));
else ok('a stamped POINTER_* is 48 bytes carrying performance.now() in ns at offset 40');
if (!w2 || w2.bytes.length !== 23) fail('stamped WHEEL is ' + (w2 && w2.bytes.length) + ' bytes, expected 23');
else if (stampAt(w2, 15) !== Math.round(6100 * 1e6)) fail('WHEEL stamp is ' + stampAt(w2, 15));
else ok('a stamped WHEEL is 23 bytes with the stamp at offset 15');
if (!k2 || k2.bytes.length !== v1KeyLen + 8) fail('stamped KEY is ' + (k2 && k2.bytes.length) + ' bytes, expected ' + (v1KeyLen + 8));
else ok('a stamped KEY appends 8 bytes past the key string');

// MOUSE_DELTA: byte 8 was the pad and is now the flags byte; bit 0 says the deltas are raw.
ctx.document.pointerLockElement = {};
ctx.RemoteInput.setPointerLockUnadjusted(true);
sent.length = 0;
firePointerMove(pointerEvent({ movementX: 5, movementY: -3 }));
const md = lastOf(OPI.MOUSE_DELTA);
if (!md) fail('no MOUSE_DELTA was sent under pointer lock');
else if (md.bytes.length !== 17) fail('stamped MOUSE_DELTA is ' + md.bytes.length + ' bytes, expected 17');
else if (md.bytes[8] !== 1) fail('MOUSE_DELTA flags byte is ' + md.bytes[8] + ', expected bit 0 (UNADJUSTED)');
else ok('MOUSE_DELTA carries the UNADJUSTED flag in byte 8 and a stamp at 9');
ctx.RemoteInput.setPointerLockUnadjusted(false);
sent.length = 0;
firePointerMove(pointerEvent({ movementX: 2, movementY: 2 }));
const md2 = lastOf(OPI.MOUSE_DELTA);
if (md2 && md2.bytes[8] !== 0) fail('MOUSE_DELTA claimed raw deltas the client is not getting');
else ok('the UNADJUSTED flag is clear when the lock did not grant it');
ctx.document.pointerLockElement = null;

// POINTER_MOVE_BATCH's tail is all three fields or none: the host reads it only when the
// stamp, the flags byte and the predicted count are all present. The count is 0 because this
// client sends measured samples only.
// Pen, not touch: touch deliberately does not batch, because a 24-byte batch sample has no room
// for the contact width and height the host injects for a touch contact.
const touch = (x, y) => pointerEvent({ pointerType: 'pen', pointerId: 11, clientX: x, clientY: y });
const withCoalesced = (e, pts) => Object.assign({}, e, { getCoalescedEvents: () => pts.map((p) => touch(p[0], p[1])) });
sent.length = 0;
now = 6200;
fire('pointerdown', touch(600, 300));
firePointerMove(withCoalesced(touch(610, 310), [[605, 305], [610, 310]]));
firePointerMove(withCoalesced(touch(620, 320), [[615, 315], [620, 320]]));
fire('pointerup', touch(620, 320));
const batch = lastOf(0x09);
if (!batch) fail('no POINTER_MOVE_BATCH was sent');
else {
  const bd = new DataView(batch.bytes.buffer, batch.bytes.byteOffset, batch.bytes.byteLength);
  const n = bd.getUint16(8, true);
  const tail = 10 + n * 24;
  if (batch.bytes.length !== tail + 10) fail('batch is ' + batch.bytes.length + ' bytes for ' + n + ' samples, expected ' + (tail + 10));
  else if (stampAt(batch, tail) !== Math.round(6200 * 1e6)) fail('batch stamp is ' + stampAt(batch, tail));
  else if (batch.bytes[tail + 9] !== 0) fail('batch claims ' + batch.bytes[tail + 9] + ' predicted samples; this client sends none');
  else ok('POINTER_MOVE_BATCH appends stamp + flags + predicted count past the samples');
}

if (!process.exitCode) console.log('\nall client stats assertions passed');
