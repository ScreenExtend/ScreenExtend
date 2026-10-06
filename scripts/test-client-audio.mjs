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

// ── audio.js: finding the outputs at the join, and when the microphone is asked for ──────
//
// The owner's rule: detect the outputs without asking wherever the platform allows; raise the
// microphone card only when that found nothing routable and a grant can change it; settle all
// of it before the offer goes out. The facts it is built on (Chrome/Edge 154, measured): with
// no grant they list exactly one output, with an empty id and label; a microphone grant lists
// them all, labelled; `permissions.query({name: 'speaker-selection'})` throws.
//
// audio.js is an IIFE that assigns `window.SEAudio`, so it is run whole against stubbed
// `navigator` surfaces and asked directly.
{
  const audioSrc = fs.readFileSync(STATIC + '/audio.js', 'utf8');

  const store = () => ({
    _v: {},
    getItem(k) { return Object.prototype.hasOwnProperty.call(this._v, k) ? this._v[k] : null; },
    setItem(k, v) { this._v[k] = String(v); },
    removeItem(k) { delete this._v[k]; },
  });
  const CHROMIUM_NO_GRANT = [{ kind: 'audiooutput', deviceId: '', label: '' }, { kind: 'audioinput', deviceId: '', label: '' }];
  const LABELLED = [
    { kind: 'audiooutput', deviceId: 'default', label: 'Default - Speakers (Realtek(R) Audio)' },
    { kind: 'audiooutput', deviceId: 'abc', label: 'Speakers (Boom Audio)' },
  ];

  // One page load. `o.enumerate(granted)` answers enumerateDevices; `o.mic` is the microphone
  // permission state; `o.gum` is what the browser's prompt does ('grant' | 'refuse');
  // `o.localStorage` is shared between page loads of one device.
  // `o.sinkApi === false` removes `setSinkId` from both prototypes and `o.mediaDevices === false`
  // removes `navigator.mediaDevices` — what a real engine does on plain HTTP (all three are
  // secure-context only) and what Safari before 18.4 does everywhere. `o.ua`/`o.platform`/
  // `o.touch` shape the device. Every `/audio-outputs` body the page sends is in `_st.posts`.
  function page(o) {
    const st = { granted: o.mic === 'granted', gumCalls: 0, queries: [], posts: [] };
    const win = { AudioContext: function () {}, isSecureContext: o.secure !== false, localStorage: o.localStorage || null };
    win.AudioContext.prototype = o.sinkApi === false ? { audioWorklet: {} } : { audioWorklet: {}, setSinkId() {} };
    const md = {
      enumerateDevices: async () => { if (o.enumerate === 'throw') throw new Error('nope'); return o.enumerate(st.granted); },
      addEventListener() {},
      ondevicechange: null,
    };
    if (o.gum !== 'none') {
      md.getUserMedia = async () => {
        st.gumCalls++;
        if (o.gum === 'refuse') { const e = new Error('Permission denied'); e.name = 'NotAllowedError'; throw e; }
        st.granted = true;
        return { getTracks: () => [{ stop() {} }] };
      };
    }
    if (o.hasSelectOut) md.selectAudioOutput = async () => null;
    const nav = {
      userAgent: o.ua || 'Mozilla/5.0 (Windows NT 10.0; Win64; x64)',
      platform: o.platform || 'Win32',
      maxTouchPoints: o.touch || 0,
      permissions: {
        query: async ({ name }) => {
          st.queries.push(name);
          if (name === 'speaker-selection') throw new TypeError("Failed to execute 'query' on 'Permissions'");
          return { state: st.granted ? 'granted' : (o.mic || 'prompt') };
        },
      },
      mediaDevices: o.mediaDevices === false ? undefined : md,
    };
    const c = {
      console: { log() {}, warn() {}, error() {} },
      performance: { now: () => 0, timeOrigin: 0 },
      setTimeout: (fn, ms) => { const t = setTimeout(fn, ms); if (t.unref) t.unref(); return t; },
      setInterval: (fn, ms) => { const t = setInterval(fn, ms); if (t.unref) t.unref(); return t; },
      clearTimeout, clearInterval,
      Promise, Float32Array, Int32Array, Uint8Array, ArrayBuffer, DataView, JSON, Math, Date,
      fetch: async (url, init) => { if (/^\/audio-outputs/.test(url)) st.posts.push(JSON.parse(init.body)); return { ok: true }; },
      navigator: nav,
      document: { visibilityState: 'visible', createElement: () => ({ setAttribute() {}, play: () => Promise.resolve(), style: {} }), body: { appendChild() {} } },
      HTMLMediaElement: function () {},
    };
    c.HTMLMediaElement.prototype = o.sinkApi === false ? {} : { setSinkId() {} };
    if (o.screen) c.screen = o.screen;
    Object.assign(c, win);
    c.window = c;
    c.self = c;
    vm.createContext(c);
    vm.runInContext(audioSrc, c, { filename: 'audio.js' });
    c.SEAudio._st = st;
    return c.SEAudio;
  }
  const chromium = (granted) => (granted ? LABELLED : CHROMIUM_NO_GRANT);
  const flush = async () => { for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r)); };

  // 0. The decision table, one row per branch.
  {
    const A = page({ enumerate: chromium });
    const base = { canSink: true, routable: 0, labelled: false, hasSelectOut: false, chooseRequested: false, secure: true, hasGUM: true, mic: 'prompt', askedBefore: false };
    const rows = [
      ['routable list, no prompt', {}, { routable: 3 }, 'report'],
      ['labelled list', {}, { routable: 2, labelled: true }, 'report'],
      ['nothing routable, a prompt can help', {}, {}, 'card'],
      ['selectAudioOutput engine, nobody asked', {}, { hasSelectOut: true }, 'none'],
      ['selectAudioOutput engine, operator asked', {}, { hasSelectOut: true, chooseRequested: true }, 'picker'],
      ['microphone denied', {}, { mic: 'denied' }, 'none'],
      ['card answered at an earlier join', {}, { askedBefore: true }, 'none'],
      ['answered earlier, operator asked again', {}, { askedBefore: true, chooseRequested: true }, 'card'],
      ['plain HTTP (no secure context)', {}, { secure: false }, 'none'],
      ['no getUserMedia', {}, { hasGUM: false }, 'none'],
      ['granted and still nothing listed', {}, { mic: 'granted' }, 'none'],
      ['no setSinkId at all', {}, { canSink: false }, 'none'],
    ];
    for (const [name, , delta, want] of rows) eq('plan: ' + name, A.outputPlan(Object.assign({}, base, delta)).action, want);
    const why = (d) => A.outputPlan(Object.assign({}, base, d)).reason;
    eq('plan reason: no sink API anywhere', why({ canSink: false }), 'no-sink-api');
    eq('plan reason: sink API hidden by plain HTTP', why({ canSink: false, insecure: true }), 'insecure-context');
    eq('plan reason: a sink API but no enumeration', why({ canSink: false, hasSinkApi: true }), 'no-enumeration');
  }

  // 1. Outputs come back without asking — nameless, but routable. No card, no prompt.
  {
    const ls = store();
    const A = page({ enumerate: () => [0, 1, 2, 3].map((i) => ({ kind: 'audiooutput', deviceId: i ? 'id' + i : 'default', label: '' })), localStorage: ls });
    let raised = 0;
    const plan = await A.prepareOutputsForJoin(() => { raised++; });
    eq('an unlabelled routable list is reported', plan.action, 'report');
    eq('and no card is raised', raised, 0);
    eq('and no microphone is asked for', A._st.gumCalls, 0);
    const out = A.getSyncInfo();
    if (out.labelsUnavailable === true) ok('the host is told the names are generic (degraded bit 9)');
    else fail('generic names were reported as real ones');
    const cache = await A.enumerateBeforeJoin();
    eq('one entry per output', cache.outputs.length, 4);
    eq('the default device is named as what it is', cache.outputs[0].label, 'Default output');
  }

  // 2. Microphone already granted: Chromium lists every output, labelled. No card.
  {
    const A = page({ enumerate: chromium, mic: 'granted', localStorage: store() });
    let raised = 0;
    const plan = await A.prepareOutputsForJoin(() => { raised++; });
    eq('a granted microphone reports the labelled list', plan.action + '/' + plan.reason, 'report/labelled');
    eq('without a card', raised, 0);
    eq('or a prompt', A._st.gumCalls, 0);
    eq('the real names are kept', A.getSyncInfo().labelsUnavailable, false);
    eq('speaker-selection throwing is an answer, not a suppressed failure', A.suppressedCount(), 0);
    eq('and it is recorded', plan.speakerSelection, 'unsupported');
  }

  // 3. Chromium with nothing granted: one empty-id entry, so the card. The join waits for it —
  //    the offer is not sent until the card is answered — and Continue reveals the list.
  {
    const ls = store();
    const A = page({ enumerate: chromium, gum: 'grant', localStorage: ls });
    let cont = null;
    let settled = false;
    const p = A.prepareOutputsForJoin((onContinue) => { cont = onContinue; }).then((r) => { settled = true; return r; });
    await flush();
    if (cont) ok('nothing routable without a grant raises the card');
    else fail('the card was not raised for an empty-id enumeration');
    eq('the join is still waiting while the card is up (no offer yet)', settled, false);
    eq('and nothing has prompted yet', A._st.gumCalls, 0);
    cont();
    const plan = await p;
    eq('Continue asks the browser for the microphone once', A._st.gumCalls, 1);
    eq('and the plan records the grant', plan.action + '/' + plan.answer, 'card/granted');
    eq('the outputs read under the grant are reported', plan.routableAfter, 2);
    eq('labelled', A.getSyncInfo().labelsUnavailable, false);
    eq('the answer is remembered', ls.getItem('se.outputs.micCardAnswer'), 'granted');
    const B = page({ enumerate: chromium, gum: 'grant', localStorage: ls });
    let raised = 0;
    const next = await B.prepareOutputsForJoin(() => { raised++; });
    eq('a one-time grant that lapsed is not asked for again', raised + '/' + next.reason, '0/asked-before');
  }

  // 4. Skip: the join continues on the default output, the host gets the choose entry, and
  //    the decline is remembered — the next join on this device does not ask again.
  {
    const ls = store();
    const A = page({ enumerate: chromium, gum: 'grant', localStorage: ls });
    const plan = await A.prepareOutputsForJoin((_c, skip) => skip());
    eq('Skip ends the wait without a prompt', plan.answer + '/' + A._st.gumCalls, 'skipped/0');
    eq('the decline is remembered on this device', ls.getItem('se.outputs.micCardAnswer'), 'skipped');
    const cache = await A.enumerateBeforeJoin();
    eq('the host is offered the choose entry', cache.outputs.length === 1 && cache.outputs[0].id, A.CHOOSE_OUTPUT);
    const B = page({ enumerate: chromium, gum: 'grant', localStorage: ls });
    let raised = 0;
    const again = await B.prepareOutputsForJoin(() => { raised++; });
    eq('a later join after a decline raises no card', raised, 0);
    eq('and says why', again.reason, 'asked-before');
  }

  // 5. Continue, then the browser's prompt is refused or dismissed: remembered the same way.
  {
    const ls = store();
    const A = page({ enumerate: chromium, gum: 'refuse', localStorage: ls });
    const plan = await A.prepareOutputsForJoin((c) => c());
    eq('a refused prompt ends the wait', plan.answer, 'refused');
    eq('and is remembered', ls.getItem('se.outputs.micCardAnswer'), 'refused');
  }

  // 6. Permission denied in the browser: a card cannot help, so none.
  {
    const A = page({ enumerate: chromium, mic: 'denied', gum: 'grant', localStorage: store() });
    let raised = 0;
    const plan = await A.prepareOutputsForJoin(() => { raised++; });
    eq('a denied microphone raises no card', raised + '/' + plan.reason, '0/mic-denied');
  }

  // 7. Plain HTTP: no secure context, so no prompt can be made. No card.
  {
    const A = page({ enumerate: chromium, secure: false, gum: 'grant', localStorage: store() });
    let raised = 0;
    const plan = await A.prepareOutputsForJoin(() => { raised++; });
    eq('no secure context raises no card', raised + '/' + plan.reason, '0/cannot-prompt');
  }

  // 8. A `selectAudioOutput()` engine with nothing listed: no microphone, ever. The choose
  //    entry is offered; when the host operator picks it the request waits for the next join,
  //    whose click opens the browser's chooser — never a tap on a live stage.
  {
    const ls = store();
    const A = page({ enumerate: () => [], hasSelectOut: true, gum: 'grant', localStorage: ls });
    let raised = 0;
    const plan = await A.prepareOutputsForJoin(() => { raised++; });
    eq('a picker engine raises no card', raised, 0);
    eq('and never asks for the microphone', A._st.gumCalls, 0);
    eq('nobody asked, so no chooser at the join', A.pickerWantedAtJoin(), false);
    eq('the plan', plan.action + '/' + plan.reason, 'none/picker-engine');
    const cache = await A.enumerateBeforeJoin();
    eq('the host is offered the choose entry', cache.outputs[0] && cache.outputs[0].id, A.CHOOSE_OUTPUT);
    eq('the operator picks it mid-session: queued, nothing raised', A.noteOutputChoiceRequest(A.CHOOSE_OUTPUT), 'queued');
    eq('every later poll carrying it is the same request', A.noteOutputChoiceRequest(A.CHOOSE_OUTPUT), 'pending');
    const B = page({ enumerate: () => [], hasSelectOut: true, gum: 'grant', localStorage: ls });
    eq('the next join opens the chooser from its click', B.pickerWantedAtJoin(), true);
    const bp = await B.prepareOutputsForJoin(() => { raised++; }, Promise.resolve(null));
    eq('the chooser, not the card', bp.action + '/' + raised + '/' + B._st.gumCalls, 'picker/0/0');
    eq('the request is answered and not repeated', B.noteOutputChoiceRequest(B.CHOOSE_OUTPUT), 'answered');
    eq('picking another output re-arms it', B.noteOutputChoiceRequest('abc'), 'cleared');
    eq('so a new request is fresh', B.noteOutputChoiceRequest(B.CHOOSE_OUTPUT), 'queued');
  }

  // 9. Chromium, declined earlier, then the operator asks: the next join asks once more.
  {
    const ls = store();
    ls.setItem('se.outputs.micCardAnswer', 'skipped');
    const A = page({ enumerate: chromium, gum: 'grant', localStorage: ls });
    eq('a Chromium page never opens a chooser', (A.noteOutputChoiceRequest(A.CHOOSE_OUTPUT), A.pickerWantedAtJoin()), false);
    const B = page({ enumerate: chromium, gum: 'grant', localStorage: ls });
    let raised = 0;
    const plan = await B.prepareOutputsForJoin((c) => { raised++; c(); });
    eq('the operator asking overrides an earlier decline, once', raised + '/' + plan.reason + '/' + plan.answer, '1/operator-asked/granted');
    const C = page({ enumerate: chromium, gum: 'grant', localStorage: ls });
    C._st.granted = false;
    let raised2 = 0;
    await C.prepareOutputsForJoin(() => { raised2++; });
    eq('and the request is not carried into the join after', raised2, 0);
  }

  // 10. The choose entry is a request, never a sink.
  {
    const A = page({ enumerate: chromium, localStorage: store() });
    const kept = await A.setSpeaker(A.CHOOSE_OUTPUT);
    if (kept === true) ok('the choose entry is never routed to as a sink');
    else fail('setSpeaker treated the choose entry as a device: ' + kept);
  }

  // 11. No localStorage at all (a private window that throws): still decides, still ends.
  {
    const A = page({ enumerate: chromium, gum: 'grant' });
    const plan = await A.prepareOutputsForJoin((_c, skip) => skip());
    eq('without storage a Skip still ends the join wait', plan.answer, 'skipped');
  }

  // 12. What `/audio-outputs` actually carries, per state. The host's device sheet says "This
  //     device's browser can't switch speakers" for `supported: false` and for nothing else, so
  //     that value is sent only by an engine with no sink API at all (no `setSinkId`, no
  //     `selectAudioOutput`) and no earpiece category. Everything else is `true`, with either a
  //     routable list or the choose entry.
  {
    const join = async (o, answer) => {
      const A = page(Object.assign({ localStorage: store() }, o));
      await A.prepareOutputsForJoin(answer || ((_c, skip) => skip()));
      A.setSpeakerIdentity('SESSION', 'token', 'join-1');
      A.postOutputs();
      await flush();
      const p = A._st.posts[A._st.posts.length - 1];
      return { A, p, ids: p ? p.outputs.map((x) => x.id) : null };
    };
    const C = page({ enumerate: chromium }).CHOOSE_OUTPUT;

    let r = await join({ enumerate: chromium, mic: 'granted' });
    eq('routable list: supported, the list', r.p.supported + '/' + r.ids.join(','), 'true/default,abc');

    r = await join({ enumerate: chromium, gum: 'grant' }, (c) => c());
    eq('Chromium, card answered with a grant: supported, the list', r.p.supported + '/' + r.ids.join(','), 'true/default,abc');

    r = await join({ enumerate: chromium, gum: 'grant' });
    eq('Chromium, nothing granted, card skipped: supported, choose entry', r.p.supported + '/' + r.ids.join(','), 'true/' + C);

    r = await join({ enumerate: chromium, gum: 'refuse' }, (c) => c());
    eq('Chromium, prompt refused: supported, choose entry', r.p.supported + '/' + r.ids.join(','), 'true/' + C);

    r = await join({ enumerate: chromium, mic: 'denied' });
    eq('Chromium, microphone denied: supported, choose entry', r.p.supported + '/' + r.ids.join(','), 'true/' + C);

    {
      const ls = store(); ls.setItem('se.outputs.micCardAnswer', 'skipped');
      r = await join({ enumerate: chromium, localStorage: ls });
      eq('Chromium, declined at an earlier join: supported, choose entry', r.p.supported + '/' + r.ids.join(','), 'true/' + C);
    }

    r = await join({ enumerate: () => [], hasSelectOut: true });
    eq('selectAudioOutput engine, nothing listed: supported, choose entry', r.p.supported + '/' + r.ids.join(','), 'true/' + C);

    r = await join({ enumerate: chromium, secure: false, sinkApi: false, mediaDevices: false });
    eq('plain HTTP (every sink API hidden): supported, choose entry', r.p.supported + '/' + r.ids.join(','), 'true/' + C);
    eq('and the plan says why', r.A.getOutputPlan().reason, 'insecure-context');

    r = await join({ enumerate: chromium, sinkApi: false });
    eq('no setSinkId and no selectAudioOutput (Safari before 18.4): NOT supported', r.p.supported + '/' + r.ids.length, 'false/0');
    eq('and the plan says why', r.A.getOutputPlan().reason, 'no-sink-api');

    r = await join({ enumerate: chromium, sinkApi: false, mediaDevices: false, ua: 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)', platform: 'iPhone', touch: 5 });
    eq('iPhone before 18.4: supported, Speaker and Earpiece', r.p.supported + '/' + r.ids.join(','), 'true/se:loudspeaker,se:earpiece');

    r = await join({ enumerate: chromium, sinkApi: false, mediaDevices: false, ua: 'Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X)', platform: 'iPad', touch: 5 });
    eq('iPad before 18.4 on HTTPS: NOT supported (the honest case)', r.p.supported + '/' + r.ids.length, 'false/0');

    // A post sent before the join had detected anything still says `true`, and the post after
    // detection replaces it with the list.
    {
      const A = page({ enumerate: chromium, mic: 'granted', localStorage: store() });
      A.setSpeakerIdentity('SESSION', 'token', 'join-1');
      A.postOutputs();
      await flush();
      eq('a post before detection is not "cannot switch"', A._st.posts[0] && A._st.posts[0].supported, true);
      await A.prepareOutputsForJoin(() => {});
      A.postOutputs();
      await flush();
      const last = A._st.posts[A._st.posts.length - 1];
      eq('and the post after detection carries the list', last.supported + '/' + last.outputs.length, 'true/2');
    }
  }
}

if (failures) {
  console.log('\n' + failures + ' audio-worker assertion(s) failed');
  process.exitCode = 1;
} else {
  console.log('\nall client audio assertions passed');
}
