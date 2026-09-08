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
if (b.length !== 96) fail('STATS is ' + b.length + ' bytes, expected the 96 of v2');
else ok('STATS is exactly the 96 bytes docs/TELEMETRY.md documents for v2');
if (b.length < 72) fail('STATS is shorter than the 72-byte floor the Rust parser requires');
else ok('STATS clears the 72-byte floor an older host reads');
if (stats.label !== 'reliable') fail('STATS went out on ' + stats.label + ', not reliable');
else ok('STATS goes on the reliable channel');

const eq = (name, got, want) => (got === want ? ok(name + ' = ' + got) : fail(name + ' = ' + got + ', expected ' + want));
eq('version', dv.getUint8(1), 2);
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

if (!process.exitCode) console.log('\nall client stats assertions passed');
