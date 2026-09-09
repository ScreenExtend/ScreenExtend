// Runs audio-worklet.js — the client's jitter buffer — under Node, and drives real decoded
// samples into it through audio-worker.js so the planar ring is exercised end to end by the two
// files that actually own it.
//
// The ring's byte layout is described identically in three files (audio.js allocates,
// audio-worker.js writes, audio-worklet.js reads) and nothing checked that they agree. A
// disagreement is silent: the writer fills bytes nobody reads and the listener hears nothing.
// So the shared-ring case here is a genuine cross-file contract test, not a restatement of the
// layout — the worker writes and the worklet reads, and the samples either come out or they do
// not. The private-ring case covers the postMessage transport the same way.
//
//   node scripts/test-client-worklet.mjs [path/to/static]
import fs from 'node:fs';
import vm from 'node:vm';

const STATIC = process.argv[2] || 'src-tauri/src/streamer/static';
const workletSrc = fs.readFileSync(STATIC + '/audio-worklet.js', 'utf8');
const workerSrc = fs.readFileSync(STATIC + '/audio-worker.js', 'utf8');

let failures = 0;
const ok = (m) => console.log('ok  - ' + m);
const fail = (m) => { failures++; console.log('FAIL- ' + m); };
const eq = (name, got, want) => (got === want ? ok(name + ' = ' + got) : fail(name + ' = ' + got + ', expected ' + want));
const near = (name, got, want, tol) => (Math.abs(got - want) <= tol ? ok(name + ' ~= ' + got) : fail(name + ' = ' + got + ', expected ' + want + ' +/- ' + tol));

const SR = 48000;
const QUANTUM = 128;
const CTRL_BYTES = 64;
const CAP = 4096; // a power of two, small enough that overrun is reachable in a test

// ── the worklet under a minimal AudioWorkletGlobalScope ──────────────────────────────────
function loadWorklet() {
  let Processor = null;
  const ctx = {
    console: { log() {}, warn() {}, error() {} },
    Math, Atomics, Int32Array, Float32Array, SharedArrayBuffer, ArrayBuffer, Error,
    sampleRate: SR,
    currentTime: 0,
    AudioWorkletProcessor: class {
      constructor() { this.port = { postMessage() {}, onmessage: null }; }
    },
    registerProcessor(name, cls) { Processor = { name, cls }; },
  };
  ctx.globalThis = ctx;
  vm.createContext(ctx);
  vm.runInContext(workletSrc, ctx, { filename: 'audio-worklet.js' });
  if (!Processor) throw new Error('audio-worklet.js registered no processor');
  return Processor;
}

const { name: processorName, cls: Processor } = loadWorklet();
eq('registered processor name', processorName, 'audio-jitter');

// A processor plus the messages it sent back to the page.
function makeProcessor(options) {
  const posted = [];
  const p = new Processor({ processorOptions: options });
  p.port.postMessage = (m) => posted.push(m);
  p.posted = posted;
  p.send = (m) => p.port.onmessage({ data: m });
  return p;
}

const outputs = () => [[new Float32Array(QUANTUM), new Float32Array(QUANTUM)]];
function pump(p, blocks) {
  const out = outputs();
  const collected = [];
  for (let i = 0; i < blocks; i++) {
    out[0][0].fill(0);
    out[0][1].fill(0);
    p.process([], out, {});
    collected.push(Float32Array.from(out[0][0]));
  }
  return collected;
}

// ── 1. the capacity invariant ────────────────────────────────────────────────────────────
// Absolute uint32 frame positions are masked with `pos & (cap-1)`, so a non-power-of-two
// capacity silently aliases two ring slots onto one.
let threw = false;
try { makeProcessor({ capacityFrames: 5000 }); } catch (_) { threw = true; }
if (threw) ok('a capacity that is not a power of two is refused, not silently aliased');
else fail('the worklet accepted a non-power-of-two capacity');

// ── 2. the shared ring, written by audio-worker.js and read by audio-worklet.js ───────────
const sab = new SharedArrayBuffer(CTRL_BYTES + CAP * 4 * 2);

const decodedPosts = [];
function loadWorker(sharedBuffer, capFrames) {
  class FakeAudioData {
    constructor(ts, value) { this.numberOfFrames = 240; this.numberOfChannels = 2; this.timestamp = ts; this.value = value; }
    copyTo(dest) { dest.fill(this.value); }
    close() {}
  }
  let nextValue = 0.5;
  const ctx = {
    console: { log() {}, warn() {}, error() {} },
    performance: { now: () => 0, timeOrigin: 1_700_000_000_000 },
    AudioDecoder: class {
      constructor(init) { this.init = init; this.state = 'unconfigured'; }
      configure() { this.state = 'configured'; }
      decode(chunk) { this.init.output(new FakeAudioData(chunk.timestamp, nextValue)); }
      close() { this.state = 'closed'; }
    },
    EncodedAudioChunk: class { constructor(o) { Object.assign(this, o); } },
    ArrayBuffer, Float32Array, Uint8Array, Int32Array, DataView, SharedArrayBuffer, Atomics, Math,
    postMessage(m) { decodedPosts.push(m); },
  };
  ctx.self = ctx;
  vm.createContext(ctx);
  vm.runInContext(workerSrc, ctx, { filename: 'audio-worker.js' });
  ctx.onmessage({ data: { type: 'init', sab: sharedBuffer, capFrames } });
  ctx.setSampleValue = (v) => { nextValue = v; };
  return ctx;
}

const HDR = 13;
let seq = 0;
function packet(worker, value) {
  worker.setSampleValue(value);
  const buf = new ArrayBuffer(HDR + 3);
  const dv = new DataView(buf);
  dv.setUint32(0, seq, true);
  dv.setUint32(4, (seq * 5 * 1e6) % 4294967296, true);
  dv.setUint32(8, Math.floor((seq * 5 * 1e6) / 4294967296), true);
  dv.setUint8(12, 0);
  new Uint8Array(buf).set([0xf8, 0xff, seq & 0xff], HDR);
  seq++;
  worker.onmessage({ data: buf });
}

const worker = loadWorker(sab, CAP);
const mode = decodedPosts.find((m) => m.type === 'mode');
if (!mode) fail('the worker announced no ring mode');
else if (mode.shared !== true) fail('the worker refused the SharedArrayBuffer it was handed');
else ok('the worker mapped the shared ring');

const reader = makeProcessor({ capacityFrames: CAP, sab });
if (!reader.shared) fail('the worklet refused the same SharedArrayBuffer the worker took');
else ok('the worklet mapped the same shared ring');

// Twelve packets is 2880 frames, above the 12 ms (576 frame) default target, so the worklet
// leaves priming and plays what the worker wrote.
for (let i = 0; i < 12; i++) packet(worker, 0.5);
const played = pump(reader, 4);
const allHalf = played.every((q) => q.every((s) => Math.abs(s - 0.5) < 1e-6));
if (allHalf) ok('a sample written by audio-worker.js comes back out of audio-worklet.js unchanged');
else fail('the shared ring round trip produced ' + played[0][0] + ', expected 0.5 — the two files disagree on the layout');

// The reader's position must be visible to the writer, or the ring fills and never drains.
const ctrl = new Int32Array(sab, 0, 16);
if ((ctrl[1] >>> 0) === 4 * QUANTUM) ok('the worklet publishes its read position for the writer');
else fail('read position is ' + (ctrl[1] >>> 0) + ', expected ' + 4 * QUANTUM);

// ── 3. overrun accounting ────────────────────────────────────────────────────────────────
// Fill past capacity: the writer must count the overrun rather than wrap over unread audio.
const beforeOverruns = ctrl[2];
for (let i = 0; i < CAP / 240 + 4; i++) packet(worker, 0.25);
if (ctrl[2] > beforeOverruns) ok('the writer counts an overrun instead of overwriting unread audio');
else fail('a full ring did not register an overrun');

// ── 4. priming: silence until the target depth, then audio ───────────────────────────────
const primer = makeProcessor({ capacityFrames: CAP });
primer.send({ type: 'samples', l: new Float32Array(QUANTUM).fill(0.9), r: new Float32Array(QUANTUM).fill(0.9) });
const early = pump(primer, 1)[0];
if (early.every((s) => s === 0)) ok('a ring below the target primes with digital silence, not a partial quantum');
else fail('the worklet played out of a ring that had not primed');

// 576 frames is the 12 ms default target; 5 quanta clears it.
for (let i = 0; i < 5; i++) {
  primer.send({ type: 'samples', l: new Float32Array(QUANTUM).fill(0.9), r: new Float32Array(QUANTUM).fill(0.9) });
}
const primed = pump(primer, 1)[0];
if (primed.every((s) => Math.abs(s - 0.9) < 1e-6)) ok('once primed the private ring plays the samples the page relayed');
else fail('the private-ring transport did not deliver: got ' + primed[0]);

// ── 5. an underrun conceals with a decaying repeat, never a square-edged silence ─────────
const starved = pump(primer, 8);
const firstStarved = starved.find((q) => q.some((s) => s !== 0.9));
if (!firstStarved) fail('the ring never actually starved');
else if (firstStarved.every((s) => s === 0)) fail('an underrun produced a square-edged silence, which is the click this code exists to avoid');
else ok('an underrun is concealed with the last quantum rather than cut to zero');
const tail = starved[starved.length - 1];
if (Math.max(...tail.map(Math.abs)) < 0.9) ok('the concealment decays rather than looping forever');
else fail('concealment is not decaying');

// ── 6. drift is trimmed only at a silence boundary ───────────────────────────────────────
// Correcting mid-phrase is audible; the whole design is to wait for a quiet moment. A ring
// above its commanded target holding loud audio must be left alone.
function depthAfter(fill, targetMs) {
  const p = makeProcessor({ capacityFrames: CAP });
  p.send({ type: 'target', targetMs });
  // 20 quanta = 2560 frames, comfortably above a 12 ms (576 frame) target.
  for (let i = 0; i < 20; i++) {
    p.send({ type: 'samples', l: new Float32Array(QUANTUM).fill(fill), r: new Float32Array(QUANTUM).fill(fill) });
  }
  const before = p.available();
  pump(p, 1);
  return { before, after: p.available(), corrections: p.corrections };
}
const loud = depthAfter(0.5, 12);
const quiet = depthAfter(0.0005, 12);
// Both consume one quantum; only the quiet one should also throw frames away.
eq('loud audio above target is played, not trimmed', loud.before - loud.after, QUANTUM);
if (quiet.before - quiet.after > QUANTUM) ok('a silent stretch above target is trimmed back toward it');
else fail('the drift trim never fired even at a silence boundary');

// A ring far above target is trimmed whatever it holds: the hard slack is a quarter of a
// second (12000 frames at 48 kHz), and that much latency is worse than one audible correction.
// The capacity has to exceed target + hard slack for the case to be reachable at all.
const BIG_CAP = 32768;
const flooded = makeProcessor({ capacityFrames: BIG_CAP });
flooded.send({ type: 'target', targetMs: 12 });
for (let i = 0; i < 130; i++) { // 16640 frames, past 576 + 12000
  flooded.send({ type: 'samples', l: new Float32Array(QUANTUM).fill(0.5), r: new Float32Array(QUANTUM).fill(0.5) });
}
const floodedBefore = flooded.available();
pump(flooded, 1);
if (flooded.available() < floodedBefore - QUANTUM) ok('a quarter second of backlog is trimmed regardless of content');
else fail('the hard slack limit did not fire on a flooded ring (' + floodedBefore + ' -> ' + flooded.available() + ')');

// ── 7. a mode mismatch falls back rather than writing into a ring nobody reads ────────────
const mismatched = makeProcessor({ capacityFrames: CAP, sab });
if (!mismatched.shared) fail('setup: the processor did not map the shared ring');
mismatched.send({ type: 'mode', shared: false });
if (mismatched.shared === false) ok('a reader on the shared ring follows a writer that could not map it');
else fail('the worklet stayed on a ring the writer will never advance');

const privateSide = makeProcessor({ capacityFrames: CAP });
privateSide.send({ type: 'mode', shared: true });
if (privateSide.posted.some((m) => m.type === 'mode-mismatch')) ok('the reverse mismatch is reported to the page, which is the only side that can fix it');
else fail('a reader that could not map the ring did not tell the page');

// ── 8. a discontinuity re-primes immediately ─────────────────────────────────────────────
const disc = makeProcessor({ capacityFrames: CAP });
for (let i = 0; i < 6; i++) {
  disc.send({ type: 'samples', l: new Float32Array(QUANTUM).fill(0.4), r: new Float32Array(QUANTUM).fill(0.4) });
}
pump(disc, 1);
if (disc.priming === false) ok('a healthy ring is not priming');
else fail('setup: the ring never primed');
disc.send({ type: 'discontinuity' });
if (disc.priming === true) ok('a host-reported discontinuity re-primes at once instead of waiting for a silence boundary');
else fail('a discontinuity did not re-prime');

// ── 9. the MessageChannel handoff is acknowledged, which is what releases the far end ─────
const ported = makeProcessor({ capacityFrames: CAP });
const port = { postMessage() {}, onmessage: null };
ported.send({ type: 'ringport', port });
if (ported.posted.some((m) => m.type === 'ringport-ok')) ok('taking the sample port is acknowledged so the worker can start using it');
else fail('the ring port was taken without acknowledgement, which strands the worker');
ported.send({ type: 'samples', l: new Float32Array(QUANTUM).fill(0.2), r: new Float32Array(QUANTUM).fill(0.2) });
if (typeof port.onmessage === 'function') ok('the worklet listens on the handed-over port');
else fail('the handed-over port has no listener');

// ── 10. stats report the commanded target, which is how audio.js measures A/V offset ─────
// The free-running target adapts on underruns, so command one and assert the worklet reports
// back the number it was given rather than its own.
const statsProc = makeProcessor({ capacityFrames: BIG_CAP });
statsProc.send({ type: 'target', targetMs: 20 });
const quantum = () => statsProc.send({ type: 'samples', l: new Float32Array(QUANTUM).fill(0.1), r: new Float32Array(QUANTUM).fill(0.1) });
for (let i = 0; i < 16; i++) quantum(); // prime past the 20 ms target
// Then one quantum in for every quantum out, which is what a healthy stream looks like.
for (let i = 0; i < 96; i++) { quantum(); pump(statsProc, 1); }
const stats = statsProc.posted.filter((m) => m.type === 'stats').pop();
if (!stats) fail('no stats were posted in 96 blocks');
else {
  near('reported target ms', stats.targetMs, 20, 0.5);
  eq('stats report no underrun on a fed ring', stats.underruns, 0);
  if (typeof stats.depthMs === 'number' && typeof stats.corrections === 'number') ok('stats carry depth and the correction count');
  else fail('stats are missing depth or corrections');
}

if (failures) {
  console.log('\n' + failures + ' audio-worklet assertion(s) failed');
  process.exitCode = 1;
} else {
  console.log('\nall client audio-worklet assertions passed');
}
