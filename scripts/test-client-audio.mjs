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

// ── audio.js: output discovery at Connect (owner spec, OWNER-SPEC-audio-discovery.md) ──────
//
// Every discovery API runs in order with its own bounded timeout and a definite result (`ok`,
// `unsupported`, `denied`, `empty`, `error`, `timeout`). A routable output — a non-empty
// `deviceId` from the chooser or the enumeration, or the iPhone's two audio-session sinks — is
// a success. Only when there is none does the microphone card appear; on Continue,
// `getUserMedia` once, enumerate again, stop the stream. If that fails too the host gets exactly
// one option, the default output, `labelled: false`. The report is final before the offer.
// The facts (Chrome/Edge 154, measured): with no grant one output with an empty id and label; a
// microphone grant lists them all, labelled; `speaker-selection` throws `TypeError`.
//
// audio.js is an IIFE that assigns `window.SEAudio`, so it is run whole against stubbed
// `navigator` surfaces and asked directly. Timers run at 1/1000 speed, so a 30 s bound is 30 ms.
{
  const audioSrc = fs.readFileSync(STATIC + '/audio.js', 'utf8');

  const CHROMIUM_NO_GRANT = [{ kind: 'audiooutput', deviceId: '', label: '' }, { kind: 'audioinput', deviceId: '', label: '' }];
  const LABELLED = [
    { kind: 'audiooutput', deviceId: 'default', label: 'Default - Speakers (Realtek(R) Audio)' },
    { kind: 'audiooutput', deviceId: 'abc', label: 'Speakers (Boom Audio)' },
  ];
  const FIREFOX_UNLABELLED = [{ kind: 'audiooutput', deviceId: 'ff1', label: '' }, { kind: 'audiooutput', deviceId: 'ff2', label: '' }];
  const never = () => new Promise(() => {});
  const err = (name, msg) => { const e = new Error(msg || name); e.name = name; return e; };
  const chromium = (granted) => (granted ? LABELLED : CHROMIUM_NO_GRANT);
  const flush = async () => { for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r)); };

  // One page load. Every option names how one API behaves:
  //   picker:       'missing' | 'throw' | 'timeout' | 'dismiss' | 'empty' | { deviceId, label }
  //   speakerPerm:  'missing' (no Permissions API) | 'throw' (TypeError, Chromium) | 'timeout' | a state
  //   mic:          a state ('granted' | 'prompt' | 'denied') | 'throw' | 'timeout'
  //   enumerate:    (granted) => devices | 'missing' | 'throw' | 'timeout'
  //   gum:          'grant' | 'refuse' | 'none' | 'timeout'
  //   sinkApi:      false removes setSinkId from both prototypes; mediaDevices: false removes it
  //   ua/platform/touch shape the device; audioSession: true adds navigator.audioSession
  // Every `/audio-outputs` body is in `_st.posts`.
  function page(o) {
    const st = { granted: o.mic === 'granted', gumCalls: 0, pickerCalls: 0, posts: [], tracksStopped: 0 };
    const win = { AudioContext: function () {}, isSecureContext: o.secure !== false, localStorage: null };
    win.AudioContext.prototype = o.sinkApi === false ? { audioWorklet: {} } : { audioWorklet: {}, setSinkId() {} };
    const md = {
      enumerateDevices: () => {
        if (o.enumerate === 'throw') return Promise.reject(err('InvalidStateError', 'nope'));
        if (o.enumerate === 'timeout') return never();
        return Promise.resolve(o.enumerate(st.granted));
      },
      addEventListener() {},
    };
    if (o.deviceChange !== false) md.ondevicechange = null;
    if (o.enumerate === 'missing') delete md.enumerateDevices;
    if (o.gum !== 'none') {
      md.getUserMedia = () => {
        st.gumCalls++;
        if (o.gum === 'refuse') return Promise.reject(err('NotAllowedError', 'Permission denied'));
        if (o.gum === 'timeout') return never();
        st.granted = true;
        return Promise.resolve({ getTracks: () => [{ stop() { st.tracksStopped++; } }] });
      };
    }
    if (o.picker && o.picker !== 'missing') {
      md.selectAudioOutput = () => {
        st.pickerCalls++;
        if (o.picker === 'throw') throw err('InvalidStateError', 'no activation');
        if (o.picker === 'timeout') return never();
        if (o.picker === 'dismiss') return Promise.reject(err('NotAllowedError', 'dismissed'));
        if (o.picker === 'empty') return Promise.resolve({ deviceId: '', label: '' });
        return Promise.resolve(o.picker);
      };
    }
    const perms = {
      query: ({ name }) => {
        const mode = name === 'speaker-selection' ? (o.speakerPerm || 'throw') : (o.mic || 'prompt');
        if (mode === 'throw') return Promise.reject(new TypeError("Failed to execute 'query' on 'Permissions'"));
        if (mode === 'timeout') return never();
        if (name === 'microphone' && st.granted) return Promise.resolve({ state: 'granted' });
        return Promise.resolve({ state: mode });
      },
    };
    const nav = {
      userAgent: o.ua || 'Mozilla/5.0 (Windows NT 10.0; Win64; x64)',
      platform: o.platform || 'Win32',
      maxTouchPoints: o.touch || 0,
      mediaDevices: o.mediaDevices === false ? undefined : md,
    };
    if (o.speakerPerm !== 'missing') nav.permissions = perms;
    if (o.audioSession) nav.audioSession = { type: 'auto' };
    const c = {
      console: { log() {}, warn() {}, error() {} },
      performance: { now: () => 0, timeOrigin: 0 },
      // Not unref'd: a bounded call's deadline is what settles it, so it must keep node alive.
      setTimeout: (fn, ms) => setTimeout(fn, Math.ceil((ms || 0) / 1000)),
      setInterval: (fn, ms) => { const t = setInterval(fn, ms); if (t.unref) t.unref(); return t; },
      clearTimeout, clearInterval,
      Promise, Float32Array, Int32Array, Uint8Array, ArrayBuffer, DataView, JSON, Math, Date,
      fetch: async (url, init) => { if (/^\/audio-outputs/.test(url)) st.posts.push(JSON.parse(init.body)); return { ok: true }; },
      navigator: nav,
      document: { visibilityState: 'visible', createElement: () => ({ setAttribute() {}, play: () => Promise.resolve(), style: {} }), body: { appendChild() {} } },
      HTMLMediaElement: function () {},
    };
    c.HTMLMediaElement.prototype = o.sinkApi === false ? {} : { setSinkId() {} };
    Object.assign(c, win);
    c.window = c;
    c.self = c;
    vm.createContext(c);
    vm.runInContext(audioSrc, c, { filename: 'audio.js' });
    c.SEAudio._st = st;
    return c.SEAudio;
  }

  // A whole Connect: the chooser started synchronously (as the click does), then the awaited
  // preparation, then — as `start()` does at the answer — the identity and the post. `card` is
  // what the user does with the microphone card: 'continue' | 'skip'.
  async function connect(o, card) {
    const A = page(o);
    let raised = 0;
    const pick = A.startPicker();
    const plan = await A.prepareOutputsForJoin((cont, skip) => { raised++; if (card === 'continue') cont(); else skip(); }, pick);
    A.setSpeakerIdentity('SESSION', 'token', 'join-1');
    A.postOutputs();
    await flush();
    const p = A._st.posts[A._st.posts.length - 1];
    return { A, plan, raised, p, ids: p ? p.outputs.map((x) => x.id).join(',') : null, d: p ? p.discovery : null };
  }

  // The baseline where every source fails: Chromium with nothing granted, no chooser.
  const NOTHING = { enumerate: chromium, picker: 'missing', gum: 'grant' };

  // 1. Each API's definite result, as recorded and posted.
  {
    const r = await connect({ enumerate: chromium, mic: 'granted', picker: 'missing', speakerPerm: 'throw' });
    eq('chooser absent → unsupported', r.d.selectAudioOutput, 'unsupported');
    eq('speaker-selection throwing TypeError → unsupported', r.d['permissions.speaker-selection'], 'unsupported');
    eq('microphone query → ok', r.d['permissions.microphone'], 'ok');
    eq('and its state is recorded', r.d['permissions.microphone.state'], 'granted');
    eq('enumeration → ok', r.d.enumerateDevices, 'ok');
    eq('AudioContext.setSinkId → ok', r.d['AudioContext.setSinkId'], 'ok');
    eq('HTMLMediaElement.setSinkId → ok', r.d['HTMLMediaElement.setSinkId'], 'ok');
    eq('audioSession absent → unsupported', r.d.audioSession, 'unsupported');
    eq('devicechange → ok', r.d.devicechange, 'ok');
    eq('no microphone step when the list was found', 'getUserMedia' in r.d, false);
    eq('the full labelled list is reported', r.ids + '/' + r.p.labelled + '/' + r.p.supported, 'default,abc/true/true');
    eq('a definite result is never counted as a suppressed failure', r.A.suppressedCount(), 0);
  }
  for (const [what, o, key, want] of [
    ['chooser throwing synchronously', { picker: 'throw' }, 'selectAudioOutput', 'error'],
    ['chooser never settling', { picker: 'timeout' }, 'selectAudioOutput', 'timeout'],
    ['chooser dismissed', { picker: 'dismiss' }, 'selectAudioOutput', 'denied'],
    ['chooser returning no device', { picker: 'empty' }, 'selectAudioOutput', 'empty'],
    ['no Permissions API', { speakerPerm: 'missing' }, 'permissions.speaker-selection', 'unsupported'],
    ['no Permissions API (microphone)', { speakerPerm: 'missing' }, 'permissions.microphone', 'unsupported'],
    ['speaker-selection never settling', { speakerPerm: 'timeout' }, 'permissions.speaker-selection', 'timeout'],
    ['microphone query throwing TypeError (Safari)', { mic: 'throw' }, 'permissions.microphone', 'unsupported'],
    ['microphone query never settling', { mic: 'timeout' }, 'permissions.microphone', 'timeout'],
    ['enumeration missing', { enumerate: 'missing' }, 'enumerateDevices', 'unsupported'],
    ['enumeration throwing', { enumerate: 'throw' }, 'enumerateDevices', 'error'],
    ['enumeration never settling', { enumerate: 'timeout' }, 'enumerateDevices', 'timeout'],
    ['enumeration with only empty-id placeholders', {}, 'enumerateDevices', 'empty'],
    ['no setSinkId anywhere', { sinkApi: false }, 'AudioContext.setSinkId', 'unsupported'],
    ['no setSinkId anywhere (element)', { sinkApi: false }, 'HTMLMediaElement.setSinkId', 'unsupported'],
    ['no devicechange', { deviceChange: false }, 'devicechange', 'unsupported'],
  ]) {
    const r = await connect(Object.assign({}, NOTHING, o), 'skip');
    eq('result: ' + what, r.d && r.d[key], want);
  }

  // 2. The microphone step is reached only when no source yields a routable output, whichever
  //    way each one failed — and never when any single one succeeds.
  for (const [what, o] of [
    ['every source failing (chooser absent, placeholders only)', {}],
    ['chooser throwing, enumeration empty', { picker: 'throw' }],
    ['chooser timing out, enumeration timing out', { picker: 'timeout', enumerate: 'timeout' }],
    ['chooser dismissed, enumeration throwing', { picker: 'dismiss', enumerate: 'throw' }],
    ['chooser empty, enumeration missing, permissions missing', { picker: 'empty', enumerate: 'missing', speakerPerm: 'missing' }],
    ['permissions timing out too', { speakerPerm: 'timeout', mic: 'timeout' }],
  ]) {
    const r = await connect(Object.assign({}, NOTHING, o), 'skip');
    eq('card raised: ' + what, r.raised, 1);
  }
  for (const [what, o] of [
    ['the chooser returns a device', { picker: { deviceId: 'pick1', label: 'Headphones' } }],
    ['the enumeration lists a routable id (Firefox, unlabelled)', { enumerate: () => FIREFOX_UNLABELLED }],
    ['the microphone was already granted', { mic: 'granted' }],
    ['an iPhone with the audio session (virtual sinks)', { sinkApi: false, enumerate: () => [], audioSession: true, ua: 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)', platform: 'iPhone', touch: 5 }],
  ]) {
    const r = await connect(Object.assign({}, NOTHING, o), 'continue');
    eq('no card when ' + what, r.raised + '/' + r.A._st.gumCalls, '0/0');
  }

  // 3. After the card: the full list when the retry finds one, the default alone otherwise.
  {
    const r = await connect(NOTHING, 'continue');
    eq('Continue → getUserMedia once', r.A._st.gumCalls, 1);
    eq('then the enumeration again, and every track stopped', r.d['enumerateDevices.afterMicrophone'] + '/' + r.A._st.tracksStopped, 'ok/1');
    eq('the full labelled list is reported', r.ids + '/' + r.p.labelled, 'default,abc/true');
    eq('the plan', r.plan.action + '/' + r.plan.answer, 'card/granted');
    eq('names are available (degraded bit 9 clear)', r.A.getSyncInfo().labelsUnavailable, false);
  }
  {
    const r = await connect(Object.assign({}, NOTHING, { enumerate: (g) => (g ? [] : CHROMIUM_NO_GRANT) }), 'continue');
    eq('a retry that finds nothing → exactly one option, the default', r.ids + '/' + r.p.outputs[0].label, '/Default output');
    eq('labelled false', r.p.labelled, false);
    eq('and still not "cannot switch" (setSinkId exists)', r.p.supported, true);
    eq('the retry is recorded as empty', r.d['enumerateDevices.afterMicrophone'], 'empty');
    eq('degraded bit 9 set', r.A.getSyncInfo().labelsUnavailable, true);
  }
  for (const [what, o, card, gumResult] of [
    ['Skip on the card', {}, 'skip', 'denied'],
    ['the browser prompt refused', { gum: 'refuse' }, 'continue', 'denied'],
    ['the browser prompt never answered', { gum: 'timeout' }, 'continue', 'timeout'],
  ]) {
    const r = await connect(Object.assign({}, NOTHING, o), card);
    eq('default only after ' + what, r.ids + '/' + r.p.labelled + '/' + r.p.supported, '/false/true');
    eq('getUserMedia recorded for ' + what, r.d.getUserMedia, gumResult);
  }
  {
    const r = await connect(Object.assign({}, NOTHING, { mic: 'denied' }), 'continue');
    eq('microphone denied: no card (a prompt cannot change it), default only', r.raised + '/' + r.ids, '0/');
    eq('and recorded', r.d.getUserMedia, 'denied');
  }

  // 4. What the host is told per device class. `supported: false` — the sheet's "This device's
  //    browser can't switch speakers" — only where there is no sink API of any kind.
  {
    let r = await connect({ enumerate: chromium, picker: 'missing', secure: false, sinkApi: false, mediaDevices: false });
    eq('plain HTTP (every sink API hidden): default only, supported', r.ids + '/' + r.p.supported + '/' + r.raised, '/true/0');
    eq('and recorded as unsupported, not a page error', r.d.enumerateDevices + '/' + r.d.getUserMedia, 'unsupported/unsupported');
    r = await connect({ enumerate: () => [], picker: 'missing', sinkApi: false, gum: 'grant' }, 'continue');
    eq('no setSinkId and no selectAudioOutput (Safari before 18.4): default only, NOT supported, no card', r.ids + '/' + r.p.supported + '/' + r.raised, '/false/0');
    r = await connect({ enumerate: () => [], picker: 'missing', sinkApi: false, mediaDevices: false, ua: 'Mozilla/5.0 (iPhone; CPU iPhone OS 16_0 like Mac OS X)', platform: 'iPhone', touch: 5 });
    eq('iPhone before 18.4: Speaker and Earpiece, supported', r.ids + '/' + r.p.supported + '/' + r.p.labelled, 'se:loudspeaker,se:earpiece/true/true');
    r = await connect({ enumerate: () => FIREFOX_UNLABELLED, picker: { deviceId: 'ff2', label: 'Headphones' } });
    eq('Firefox: the chooser\'s pick is a labelled source and is not overwritten', r.ids + '/' + r.p.labelled, 'ff2,ff1/true');
    eq('and the pick is the preferred sink, applied when the graph exists', r.A.getSyncInfo().currentSink, null);
    r = await connect({ enumerate: () => FIREFOX_UNLABELLED, picker: 'dismiss' });
    eq('Firefox, chooser dismissed: the unlabelled ids, positional names, labelled false', r.ids + '/' + r.p.outputs.map((x) => x.label).join(',') + '/' + r.p.labelled, 'ff1,ff2/Output 1,Output 2/false');
  }

  // 5. The report is final before the offer: when the preparation resolves, the body the host
  //    will get is fixed; the card wait holds it open, and nothing is posted before the join.
  {
    const A = page(NOTHING);
    let cont = null;
    let settled = false;
    const p = A.prepareOutputsForJoin((c) => { cont = c; }, A.startPicker()).then((r) => { settled = true; return r; });
    await flush();
    await new Promise((r) => setTimeout(r, 40));
    eq('with the card up the preparation is still open (no offer can be sent)', settled, false);
    eq('and nothing has prompted yet', A._st.gumCalls, 0);
    cont();
    const plan = await p;
    eq('Continue resolves it with a definite plan', plan.action, 'card');
    eq('nothing was posted before the join existed', A._st.posts.length, 0);
    const discovery = JSON.stringify(A.getDiscovery());
    A.setSpeakerIdentity('SESSION', 'token', 'join-1');
    A.postOutputs();
    await flush();
    eq('the post at the answer carries the discovery fixed before the offer', JSON.stringify(A._st.posts[0].discovery), discovery);
  }
  {
    // A post that somehow precedes the preparation is the default alone, never "cannot switch".
    const A = page({ enumerate: chromium, mic: 'granted', picker: 'missing' });
    A.setSpeakerIdentity('SESSION', 'token', 'join-1');
    A.postOutputs();
    await flush();
    eq('a post before discovery: default only, supported', A._st.posts[0].outputs.map((x) => x.id).join(',') + '/' + A._st.posts[0].supported, '/true');
  }

  // 6. The old "Choose a speaker…" entry, still saved on some host rows, is ignored.
  {
    const A = page(NOTHING);
    eq('the choose request is ignored', A.noteOutputChoiceRequest(A.CHOOSE_OUTPUT), 'ignored');
    const kept = await A.setSpeaker(A.CHOOSE_OUTPUT);
    if (kept === true) ok('the choose entry is never routed to as a sink');
    else fail('setSpeaker treated the choose entry as a device: ' + kept);
  }
}

if (failures) {
  console.log('\n' + failures + ' audio-worker assertion(s) failed');
  process.exitCode = 1;
} else {
  console.log('\nall client audio assertions passed');
}
