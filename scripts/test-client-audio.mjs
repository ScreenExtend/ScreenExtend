// Runs audio-worker.js under Node with stub WebCodecs classes and drives real DataChannel
// frames through it. What it protects is the packet-redundancy split: the host appends the
// previous frame's Opus payload behind a u16 length prefix when the link is losing packets
// (streamer/audio/protocol.rs), and a copy that is played when its frame already arrived is
// worse than no redundancy at all — it doubles the audio.
import fs from 'node:fs';
import vm from 'node:vm';

const STATIC = process.argv[2] || 'src-tauri/src/streamer/static';
const src = fs.readFileSync(STATIC + '/audio-worker.js', 'utf8');

let failures = 0;
const ok = (m) => console.log('ok  - ' + m);
const fail = (m) => { failures++; console.log('FAIL- ' + m); };
const eq = (name, got, want) => (got === want ? ok(name + ' = ' + got) : fail(name + ' = ' + got + ', expected ' + want));

const HDR = 13;
const FLAG_DISCONTINUITY = 1 << 1;
const FLAG_REDUNDANT = 1 << 2;

const decoded = [];   // timestamps handed to the decoder, in submission order
const posted = [];    // messages the worker sent to the page

let clock = 0;

class FakeAudioData {
  constructor(ts) {
    this.numberOfFrames = 240; // 5 ms at 48 kHz
    this.numberOfChannels = 2;
    this.timestamp = ts;
  }
  copyTo(dest) { dest.fill(0.25); }
  close() {}
}

class FakeAudioDecoder {
  constructor(init) { this.init = init; this.state = 'unconfigured'; }
  configure() { this.state = 'configured'; }
  decode(chunk) {
    decoded.push(chunk.timestamp);
    // Synchronous output keeps the ordering assertions exact and exercises copyTo.
    this.init.output(new FakeAudioData(chunk.timestamp));
  }
  close() { this.state = 'closed'; }
}

const ctx = {
  console: { log() {}, warn() {}, error() {} },
  performance: { now: () => clock, timeOrigin: 1_700_000_000_000 },
  AudioDecoder: FakeAudioDecoder,
  EncodedAudioChunk: class { constructor(o) { Object.assign(this, o); } },
  // The worker gates on `d instanceof ArrayBuffer`, and instanceof does not cross realms —
  // the buffers below have to be built from the same constructor the worker sees.
  ArrayBuffer,
  Float32Array,
  Uint8Array,
  DataView,
  Math,
  postMessage(m) { posted.push(m); },
};
ctx.self = ctx;
vm.createContext(ctx);
vm.runInContext(src, ctx, { filename: 'audio-worker.js' });

if (typeof ctx.onmessage !== 'function') { fail('the worker installed no onmessage'); process.exit(1); }

const send = (data) => ctx.onmessage({ data });

function packet(seq, captureNs, flags, primary, previous) {
  const extra = previous ? 2 + previous.length : 0;
  const buf = new ArrayBuffer(HDR + primary.length + extra);
  const dv = new DataView(buf);
  dv.setUint32(0, seq >>> 0, true);
  dv.setUint32(4, captureNs % 4294967296, true);
  dv.setUint32(8, Math.floor(captureNs / 4294967296), true);
  dv.setUint8(12, previous ? (flags | FLAG_REDUNDANT) : flags);
  const bytes = new Uint8Array(buf);
  if (previous) {
    dv.setUint16(HDR, primary.length, true);
    bytes.set(primary, HDR + 2);
    bytes.set(previous, HDR + 2 + primary.length);
  } else {
    bytes.set(primary, HDR);
  }
  return buf;
}

const body = (n) => new Uint8Array([0xf8, 0xff, n & 0xff]);
const NS = 1_000_000; // one millisecond
const capture = (seq) => seq * 5 * NS;

send({ type: 'init', sab: null, capFrames: 65536 });
const mode = posted.find((m) => m.type === 'mode');
if (!mode) fail('the worker did not announce its ring mode');
else if (mode.shared !== false) fail('the worker claimed a shared ring it was not given');
else ok('the worker announces its ring mode so the reader can follow it');

// 1. A plain packet decodes exactly once, at its own capture stamp.
decoded.length = 0;
send(packet(10, capture(10), 0, body(1)));
eq('plain packet decodes', decoded.length, 1);
eq('plain packet timestamp us', decoded[0], capture(10) / 1000);

// 2. A redundant packet with no gap: the copy is for a frame we already have, so it is
//    discarded, not played. Playing it would emit the same 5 ms twice.
decoded.length = 0;
send(packet(11, capture(11), 0, body(2), body(1)));
eq('redundant packet with no loss decodes once', decoded.length, 1);
eq('and it is the primary', decoded[0], capture(11) / 1000);

// 3. A redundant packet after one lost frame: the copy fills the hole and is decoded first.
decoded.length = 0;
send(packet(13, capture(13), 0, body(4), body(3)));
eq('a single loss is repaired by the copy', decoded.length, 2);
eq('the recovered frame is decoded first', decoded[0], capture(12) / 1000);
eq('then the primary', decoded[1], capture(13) / 1000);
if (decoded[0] < decoded[1]) ok('the two are submitted in timestamp order');
else fail('the recovered frame was submitted after the primary');

// 4. A duplicate and a late packet are both ignored.
decoded.length = 0;
send(packet(13, capture(13), 0, body(4)));
eq('a duplicate seq is ignored', decoded.length, 0);
send(packet(9, capture(9), 0, body(9)));
eq('a late seq is ignored', decoded.length, 0);

// 5. A discontinuity is an explicit resync point, not loss: no concealment, and the worklet
//    is told so it can re-derive its depth.
posted.length = 0;
decoded.length = 0;
clock += 500;
send(packet(60, capture(60), FLAG_DISCONTINUITY, body(6)));
eq('a discontinuity still decodes its own payload', decoded.length, 1);
if (posted.some((m) => m.type === 'discontinuity')) ok('a discontinuity is relayed to the worklet');
else fail('the discontinuity flag was not relayed');

// 6. The counters: one copy used (step 3), one discarded (step 2).
clock += 500;
send(packet(61, capture(61), 0, body(7)));
const sync = posted.filter((m) => m.type === 'sync').pop();
if (!sync) fail('no sync message carried the redundancy counters');
else {
  eq('redundant copies used', sync.redundantUsed, 1);
  eq('redundant copies discarded', sync.redundantDiscarded, 1);
}

// 7. The sequence gate, over the shipped implementation.
//
//    These five cases used to live in `src-tauri/src/streamer/audio/tests.rs` against a
//    `SeqGate` struct defined in that test file and used nowhere else — and the copy had
//    already drifted from the real gate at one value (`0x7FFF_FFFF`). The host is the producer
//    and never dedups, so the only sequence gate in the product is the one below.
// A fresh module scope per case, because `lastSeq` persists across messages by design.
function freshWorker() {
  const local = { decoded: [], posted: [] };
  const c = {
    console: { log() {}, warn() {}, error() {} },
    performance: { now: () => clock, timeOrigin: 1_700_000_000_000 },
    AudioDecoder: class {
      constructor(init) { this.init = init; this.state = 'unconfigured'; }
      configure() { this.state = 'configured'; }
      decode(chunk) { local.decoded.push(chunk.timestamp); this.init.output(new FakeAudioData(chunk.timestamp)); }
      close() { this.state = 'closed'; }
    },
    EncodedAudioChunk: class { constructor(o) { Object.assign(this, o); } },
    ArrayBuffer, Float32Array, Uint8Array, DataView, Math,
    postMessage(m) { local.posted.push(m); },
  };
  c.self = c;
  vm.createContext(c);
  vm.runInContext(src, c, { filename: 'audio-worker.js' });
  c.onmessage({ data: { type: 'init', sab: null, capFrames: 65536 } });
  local.send = (seq) => c.onmessage({ data: packet(seq, capture(seq), 0, body(seq & 0xff)) });
  return local;
}

{
  const w = freshWorker();
  for (let s = 0; s < 10; s++) w.send(s);
  eq('an in-order run is accepted whole', w.decoded.length, 10);
}
{
  const w = freshWorker();
  w.send(5);
  w.send(5);
  eq('a duplicate seq decodes once', w.decoded.length, 1);
}
{
  const w = freshWorker();
  w.send(10); w.send(11); w.send(9); w.send(12);
  eq('a packet older than the last played is dropped', w.decoded.length, 3);
}
{
  // A gap is not a stall: 8 is newer than 5, so it plays. Concealment covers 6 and 7.
  const w = freshWorker();
  w.send(5); w.send(8);
  if (w.decoded.length >= 2) ok('a forward gap does not stall the gate');
  else fail('a forward reorder gap stalled: ' + w.decoded.length + ' decodes');
}
{
  // u32 wraparound. The cutoff is `>= 0x80000000` on the unsigned difference, so 0 after
  // 0xFFFFFFFF is newer and 0xFFFFFFFF after 1 is late. This is the exact value the deleted
  // Rust mirror disagreed on.
  const w = freshWorker();
  w.send(0xFFFFFFFE); w.send(0xFFFFFFFF); w.send(0); w.send(1);
  const afterWrap = w.decoded.length;
  w.send(0xFFFFFFFF);
  eq('wraparound to 0 is newer, and the pre-wrap seq is then late', w.decoded.length, afterWrap);
  if (afterWrap >= 4) ok('all four packets across the wrap were accepted');
  else fail('only ' + afterWrap + ' packets survived the wrap');
}
{
  // The half-range boundary itself: a difference of exactly 0x7FFFFFFF is newer (the drifted
  // Rust mirror called it late), and 0x80000000 is late.
  const w = freshWorker();
  w.send(0);
  w.send(0x7FFFFFFF);
  eq('a gap of exactly 0x7FFFFFFF is newer', w.decoded.length, 2);
  const w2 = freshWorker();
  w2.send(0);
  w2.send(0x80000000);
  eq('a gap of exactly 0x80000000 is late', w2.decoded.length, 1);
}

// 8. A truncated redundant body is refused rather than half-parsed.
decoded.length = 0;
const bad = new ArrayBuffer(HDR + 2 + 4);
const bv = new DataView(bad);
bv.setUint32(0, 70, true);
bv.setUint8(12, FLAG_REDUNDANT);
bv.setUint16(HDR, 999, true); // claims more primary bytes than the frame holds
send(bad);
eq('a truncated redundant frame decodes nothing', decoded.length, 0);

// ── audio.js: when the microphone is asked for at all ───────────────────────────────────
//
// The rule is that a permission prompt is the last resort, not the first: enumerating the
// outputs is tried without any grant, and only an enumeration that produced *nothing* is
// answered with the card. Unlabelled ids are a working list — `setSinkId` takes the bare id —
// so a laptop that reports four nameless outputs is never prompted, and the host is told the
// names are this page's own numbering instead.
//
// audio.js is an IIFE that assigns `window.SEAudio`, so it is run whole against stubbed
// `navigator` surfaces and asked directly.
{
  const audioSrc = fs.readFileSync(STATIC + '/audio.js', 'utf8');

  // `enumerateDevices` is the only thing each case varies; everything else is the shape of a
  // browser with `setSinkId` and no `selectAudioOutput` — the majority case, and the one the
  // old rule prompted on.
  function speakersFor(enumerate, opts) {
    const o = opts || {};
    const win = {
      AudioContext: function () {},
      isSecureContext: true,
    };
    win.AudioContext.prototype = { audioWorklet: {}, setSinkId() {} };
    const nav = {
      userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64)',
      platform: 'Win32',
      maxTouchPoints: 0,
      permissions: { query: async () => ({ state: o.micGranted ? 'granted' : 'prompt' }) },
      mediaDevices: {
        enumerateDevices: enumerate,
        addEventListener() {},
        ondevicechange: null,
      },
    };
    if (o.hasSelectOut) nav.mediaDevices.selectAudioOutput = async () => null;
    const c = {
      console: { log() {}, warn() {}, error() {} },
      performance: { now: () => 0, timeOrigin: 0 },
      // Unref'd: `startDeviceWatch()` arms a 30 s device poll that is right in a browser and
      // would otherwise hold this process open long after the assertions have run.
      setTimeout: (fn, ms) => { const t = setTimeout(fn, ms); if (t.unref) t.unref(); return t; },
      setInterval: (fn, ms) => { const t = setInterval(fn, ms); if (t.unref) t.unref(); return t; },
      clearTimeout, clearInterval,
      Promise, Float32Array, Int32Array, Uint8Array, ArrayBuffer, DataView, JSON, Math, Date,
      fetch: async () => ({ ok: true }),
      navigator: nav,
      document: { visibilityState: 'visible', createElement: () => ({ setAttribute() {}, play: () => Promise.resolve(), style: {} }), body: { appendChild() {} } },
      HTMLMediaElement: function () {},
    };
    c.HTMLMediaElement.prototype = { setSinkId() {} };
    Object.assign(c, win);
    c.window = c;
    c.self = c;
    vm.createContext(c);
    vm.runInContext(audioSrc, c, { filename: 'audio.js' });
    return c.SEAudio;
  }

  const outs = (n) => Array.from({ length: n }, (_, i) => ({ kind: 'audiooutput', deviceId: i ? 'id' + i : 'default', label: '' }));

  // 1. Outputs came back — nameless, but four of them. Nothing is asked.
  {
    const A = speakersFor(async () => outs(4));
    const need = await A.needsMicPermission();
    if (need === false) ok('an enumeration that returns unlabelled outputs asks for no microphone');
    else fail('unlabelled outputs still asked for the microphone');
    const cache = await A.enumerateBeforeJoin(() => { fail('the card was raised for a list that already worked'); });
    eq('and the list it built has one entry per output', cache.outputs.length, 4);
    if (cache.outputs.every((o) => !!o.label)) ok('every entry carries a name the host can render');
    else fail('an entry was posted with no label at all: ' + JSON.stringify(cache.outputs));
    if (cache.outputs[0].label === 'Default output') ok('the default device is named as what it is');
    else fail('the default device was named ' + cache.outputs[0].label);
    if (A.getSyncInfo().labelsUnavailable === true) ok('and the host is told the names are generic (degraded bit 9)');
    else fail('generic names were reported as real ones');
  }

  // 2. Labelled outputs: nothing is asked, and nothing is renamed.
  {
    const A = speakersFor(async () => [{ kind: 'audiooutput', deviceId: 'default', label: 'Speakers (Realtek)' }]);
    const need = await A.needsMicPermission();
    if (need === false) ok('a labelled enumeration asks for no microphone either');
    else fail('a labelled enumeration asked for the microphone');
    const cache = await A.enumerateBeforeJoin(() => { fail('the card was raised for a labelled list'); });
    eq('the real name is kept', cache.outputs[0].label, 'Speakers (Realtek)');
    if (A.getSyncInfo().labelsUnavailable === false) ok('and the host is told the names are real');
    else fail('real names were reported as generic');
  }

  // 3. Zero outputs: this is the case a grant can actually change.
  {
    const A = speakersFor(async () => []);
    const need = await A.needsMicPermission();
    if (need === true) ok('an enumeration that returns no outputs at all does ask');
    else fail('an empty enumeration did not ask for the microphone');
    let raised = 0;
    await A.enumerateBeforeJoin((_cont, skip) => { raised++; skip(); });
    eq('and the card is raised exactly once', raised, 1);
  }

  // 4. Enumeration unsupported — it throws — is the same answer.
  {
    const A = speakersFor(async () => { throw new Error('not supported here'); });
    const need = await A.needsMicPermission();
    if (need === true) ok('an enumeration that throws asks, the same as one that returns nothing');
    else fail('a throwing enumeration did not ask for the microphone');
  }

  // 5. A permission-free picker is never asked, whatever the enumeration says.
  {
    const A = speakersFor(async () => [], { hasSelectOut: true });
    const need = await A.needsMicPermission();
    if (need === false) ok('an engine with selectAudioOutput() is never asked, even with no outputs');
    else fail('a selectAudioOutput engine was asked for the microphone');
  }

  // 6. A grant already given is not asked for again.
  {
    const A = speakersFor(async () => [], { micGranted: true });
    const need = await A.needsMicPermission();
    if (need === false) ok('a microphone already granted is not asked for a second time');
    else fail('an existing grant was asked for again');
  }
}

if (failures) {
  console.log('\n' + failures + ' audio-worker assertion(s) failed');
  process.exitCode = 1;
} else {
  console.log('\nall client audio assertions passed');
}
