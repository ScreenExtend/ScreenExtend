// Runs transform-worker.js (the client's WebCodecs video path) under Node with stub WebCodecs
// classes and a stub encoded-frame stream.
//
// It asserts what a browser cannot be relied on to reveal: that the RTP timestamp inverts back
// to a host capture time across the 32-bit wrap (~13.25 h of host uptime), that a delta before
// the resume keyframe is dropped and one after the wrap is not, that backpressure is measured as
// decodes without outputs rather than as `decodeQueueSize`, and that every keyframe tier is
// reported honestly.
//
//   node scripts/test-client-transform.mjs [path/to/static]
import fs from 'node:fs';
import vm from 'node:vm';

const STATIC = process.argv[2] || 'src-tauri/src/streamer/static';
const src = fs.readFileSync(STATIC + '/transform-worker.js', 'utf8');

let failures = 0;
const ok = (m) => console.log('ok  - ' + m);
const fail = (m) => { failures++; console.log('FAIL- ' + m); };
const eq = (name, got, want) => (got === want ? ok(name + ' = ' + got) : fail(name + ' = ' + got + ', expected ' + want));
const near = (name, got, want, tol) => (Math.abs(got - want) <= tol ? ok(name + ' ~= ' + got.toFixed(3)) : fail(name + ' = ' + got + ', expected ' + want + ' +/- ' + tol));

const RTP_HZ = 90000;
const TIME_ORIGIN = 1_700_000_000_000;

// One worker instance plus everything it was handed and everything it sent back.
function makeWorker(opts) {
  const o = opts || {};
  const posted = [];
  const decoded = [];
  const painted = [];
  // Every paint with the stub clock at the moment it happened; `paintMs` is what each costs.
  const paints = [];
  let sendKeyCalls = 0;
  let resets = 0;
  let configures = 0;
  let clock = o.startClock === undefined ? 10_000 : o.startClock;
  let decoderState = 'unconfigured';
  let configuredWith = null;
  const probed = [];
  // The queue the pump reads from; `feed` pushes and the pump drains asynchronously.
  const queue = [];
  let waiting = null;
  let closed = false;
  // `holdOutputs`: the decoder accepts chunks but returns nothing until `releaseOutputs()`,
  // which emits them in decode order (a decoder busy on a large keyframe).
  let holding = false;
  const held = [];
  let decoderInit = null;
  let stalled = false;
  const timers = [];
  let timerIds = 0;

  class FakeVideoFrame {
    constructor(ts) { this.timestamp = ts; this.displayWidth = 1280; this.displayHeight = 720; this.codedWidth = 1280; this.codedHeight = 720; }
    close() {}
  }

  const ctx = {
    console: { log() {}, warn() {}, error() {} },
    performance: { now: () => clock, timeOrigin: TIME_ORIGIN },
    // Real timers, on the macrotask queue: `waitForCodecHint` races the page's hint
    // against a 600 ms deadline, and a setTimeout that never fires means the decoder is
    // never configured at all.
    // A timer of a second or more, or any timer with `timed`, runs on the stub clock: it fires
    // when `tick` moves the clock past its due time, so a case can tell 4.9 s from 5 s.
    setTimeout: (fn, ms) => {
      if (ms >= 1000 || o.timed) { const id = ++timerIds; timers.push({ id, due: clock + (ms || 0), fn }); return id; }
      setImmediate(fn);
      return 0;
    },
    clearTimeout(id) { const i = timers.findIndex((t) => t.id === id); if (i >= 0) timers.splice(i, 1); },
    queueMicrotask: (fn) => Promise.resolve().then(fn),
    Promise, Map, Array, Math, Float64Array, Uint8Array, Error, isFinite, String, Object,
    VideoDecoder: class {
      constructor(init) {
        this.init = init;
        decoderInit = init;
        this.decodeQueueSize = 0;
        Object.defineProperty(this, 'state', { get: () => decoderState });
      }
      configure(cfg) {
        configuredWith = cfg; decoderState = 'configured'; configures++; clock += o.configureMs || 0;
        // `stallUntilReconfigured`: the first configuration takes decodes and answers nothing;
        // configuring it again is what brings it back.
        stalled = (!!o.stallUntilReconfigured && configures === 1) || !!o.stallAlways;
      }
      decode(chunk) {
        decoded.push({ type: chunk.type, timestamp: chunk.timestamp });
        // A decoder that accepts a chunk and never produces an output is the real failure
        // mode this path guards against: `decodeQueueSize` reads zero the whole time.
        if (o.swallowDeltas && chunk.type !== 'key') return;
        if (stalled) { this.decodeQueueSize++; return; }
        this.decodeQueueSize = 0;
        if (holding) { held.push(chunk.timestamp); return; }
        this.init.output(new FakeVideoFrame(chunk.timestamp));
      }
      // Discards what the decoder holds, as a real reset does, and costs the thread `resetMs`.
      reset() { held.length = 0; resets++; clock += o.resetMs || 0; }
      close() { decoderState = 'closed'; }
    },
    EncodedVideoChunk: class { constructor(init) { Object.assign(this, init); } },
    VideoFrame: FakeVideoFrame,
  };
  if (o.trackGenWriteMs !== undefined) {
    // A track generator whose sink takes `trackGenWriteMs` of this thread per frame.
    ctx.VideoTrackGenerator = class {
      constructor() {
        this.track = { kind: 'video', stop() {} };
        this.writable = { getWriter: () => ({ write: (f) => { painted.push(f.timestamp); paints.push({ ts: f.timestamp, at: clock }); clock += o.trackGenWriteMs; return Promise.resolve(); }, close: () => Promise.resolve() }) };
      }
    };
  }
  ctx.VideoDecoder.isConfigSupported = async (cfg) => {
    probed.push(cfg);
    if (o.rejectSized && cfg.codedWidth) return { supported: false };
    if (o.supportCheckNeverAnswers) return new Promise(() => {});
    // `supportCheckMs`: the first check answers that long after it was asked, on the stub clock.
    if (o.supportCheckMs && probed.length === 1) {
      await new Promise((resolve) => ctx.setTimeout(resolve, o.supportCheckMs));
    }
    return { supported: true, config: cfg };
  };
  ctx.self = ctx;
  ctx.postMessage = (m) => posted.push(m);
  Object.defineProperty(ctx, 'onmessage', { writable: true, value: null });
  vm.createContext(ctx);
  vm.runInContext(src, ctx, { filename: 'transform-worker.js' });

  const readable = {
    getReader() {
      return {
        read() {
          if (queue.length) return Promise.resolve({ value: queue.shift(), done: false });
          if (closed) return Promise.resolve({ value: undefined, done: true });
          return new Promise((resolve) => { waiting = resolve; });
        },
      };
    },
  };

  return {
    ctx,
    posted,
    decoded,
    painted,
    paints,
    probed,
    get sendKeyCalls() { return sendKeyCalls; },
    get resets() { return resets; },
    get configures() { return configures; },
    get configuredWith() { return configuredWith; },
    setClock(ms) { clock = ms; },
    holdOutputs(on) { holding = on; },
    releaseOutputs(only) {
      const n = only === undefined ? held.length : Math.min(only, held.length);
      for (const ts of held.splice(0, n)) decoderInit.output(new FakeVideoFrame(ts));
    },
    get heldCount() { return held.length; },
    // The decoder silently discards the oldest picture it holds (a delta it could not use).
    dropOutputs(n) { held.splice(0, n); },
    advance(ms) { clock += ms; },
    // Move the stub clock forward `ms`, firing every timer that falls due on the way, in order.
    async tick(ms) {
      const end = clock + ms;
      for (;;) {
        let next = null;
        for (const t of timers) if (t.due <= end && (!next || t.due < next.due)) next = t;
        if (!next) break;
        timers.splice(timers.indexOf(next), 1);
        if (next.due > clock) clock = next.due;
        next.fn();
        for (let i = 0; i < 12; i++) await new Promise((r) => setImmediate(r));
      }
      if (end > clock) clock = end;
    },
    send(msg) { ctx.onmessage({ data: msg }); },
    // The canvas the page hands over. `getContext` has to succeed or the worker reports
    // `no 2d context` and never claims a render tier.
    attachCanvas() {
      const canvas = {
        width: 0, height: 0,
        getContext: () => ({ drawImage: (f) => { painted.push(f.timestamp); paints.push({ ts: f.timestamp, at: clock }); clock += o.paintMs || 0; } }),
      };
      ctx.onmessage({ data: { type: 'canvas', canvas } });
      return canvas;
    },
    startPump(writable) { ctx.onmessage({ data: { type: 'encodedstream', readable, writable } }); },
    // The receiver's writable: everything the worker hands back to libwebrtc.
    receiverWritable() {
      const written = [];
      return { written, getWriter: () => ({ write: (f) => { written.push(f.type + '@' + f.timestamp); return Promise.resolve(); } }) };
    },
    startScriptTransform(writable) {
      ctx.onrtctransform({ transformer: { readable, writable, sendKeyFrameRequest: () => { sendKeyCalls++; return Promise.resolve(); } } });
    },
    feed(frame) {
      if (waiting) { const w = waiting; waiting = null; w({ value: frame, done: false }); }
      else queue.push(frame);
    },
    // The pump is async and the configure races a timer, so give both several macrotask
    // turns to drain rather than guessing at a fixed number.
    async settle() { for (let i = 0; i < 12; i++) await new Promise((r) => setImmediate(r)); },
  };
}

const frame = (type, ts) => ({ type, timestamp: ts, data: new Uint8Array([0, 0, 0, 1, type === 'key' ? 0x65 : 0x41]) });
const lastOf = (posted, type) => { for (let i = posted.length - 1; i >= 0; i--) if (posted[i].type === type) return posted[i]; return null; };

// 1. A delta before the first keyframe is never decoded.
{
  const w = makeWorker();
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.feed(frame('delta', 1000));
  w.feed(frame('delta', 4000));
  await w.settle();
  eq('deltas before the first keyframe are not decoded', w.decoded.length, 0);
  if (lastOf(w.posted, 'firstframe')) ok('the first frame off the wire is reported to the page');
  else fail('no firstframe message');

  w.feed(frame('key', 7000));
  w.feed(frame('delta', 10000));
  await w.settle();
  eq('the keyframe and the delta after it are decoded', w.decoded.length, 2);
  eq('the first decode is the keyframe', w.decoded[0].type, 'key');
  eq('the picture reaches the canvas', w.painted.length, 2);
}

// 2. The RTP timestamp inverts back to a host capture time, across the wrap. This is the A/V
//    sync contract: the host packetizes H.264 itself so the RTP timestamp carries
//    `host_ns_to_rtp90k(capture_ns)`, and this is the only code that inverts it. A wrap
//    mishandled here reads as thirteen hours of display lag.
//
//    The avsync post is rate limited to one per 200 ms, so every frame below is fed at least
//    that far apart on the stub clock.
{
  const w = makeWorker();
  w.attachCanvas();
  w.startPump();
  await w.settle();
  // Host clock is 20 s ahead of the page's local absolute clock.
  const OFFSET = 20_000;
  w.send({ type: 'clockoffset', offsetMs: OFFSET });

  const ticks = (sec) => Math.round(sec * RTP_HZ) >>> 0;
  // A frame the host captured at t = 1000 s, painted at local performance.now() = 1000 ms.
  w.setClock(1000);
  w.feed(frame('key', ticks(1000)));
  await w.settle();
  const sync = lastOf(w.posted, 'avsync');
  if (!sync) fail('no avsync was reported');
  else {
    // videoDelay = (timeOrigin + now) - (hostCaptureMs - offset), and the EMA seeds with its
    // first sample, so this is exact rather than smoothed.
    near('video display lag ms', sync.videoDelayMs, TIME_ORIGIN + 1000 - (1000 * 1000 - OFFSET), 0.001);
    if (sync.clockValid) ok('the lag is reported as valid once the clock offset has landed');
    else fail('clockValid was not set after a clockoffset message');
  }

  // Now wrap. 0xFFFFFF00 then 0x00000100 is one wrap forward, 512 ticks (5.7 ms) apart. Get
  // the unwrap wrong and the reported lag moves by 2^32 / 90000 = 47721 s.
  w.setClock(1300);
  w.feed(frame('delta', 0xFFFFFF00));
  await w.settle();
  const beforeWrap = lastOf(w.posted, 'avsync').videoDelayMs;
  w.setClock(1600);
  w.feed(frame('delta', 0x00000100));
  await w.settle();
  const afterWrap = lastOf(w.posted, 'avsync').videoDelayMs;
  if (Math.abs(afterWrap - beforeWrap) < 60_000) ok('the 32-bit RTP wrap does not move the reported lag by 13 hours');
  else fail('the wrap moved the reported lag by ' + ((afterWrap - beforeWrap) / 1000).toFixed(0) + ' s');
}

// 3. The resume-keyframe gate survives the wrap too. `renderFromTs` is the raw stamp of the
//    keyframe the stream resumed from. A plain `>=` comparison would drop every frame for the
//    rest of the session once the stamp wraps past it, silently, thirteen hours in.
{
  const w = makeWorker();
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.feed(frame('key', 0xFFFFFF00));
  await w.settle();
  const paintedAfterKey = w.painted.length;
  if (paintedAfterKey === 0) fail('setup: the resume keyframe did not paint');
  w.feed(frame('delta', 0x00000200)); // wrapped, but newer
  await w.settle();
  if (w.painted.length > paintedAfterKey) ok('a frame whose stamp wrapped past the resume keyframe still paints');
  else fail('the wrapped delta was treated as older than the resume keyframe and dropped');
}

// 4. Backpressure is a decoder that stopped, not one that is merely pipelined. `decodeQueueSize`
//    never leaves 0 in the stub, so the in-flight count is what has to fire. But the count alone
//    is also what a healthy pipelined hardware decoder looks like, and the response to a backlog
//    is severe: every delta is thrown away until a keyframe arrives, and the host gates keyframes
//    to one per 200 ms. So "no output for BACKLOG_STALL_MS" is the load-bearing half.
{
  const w = makeWorker({ swallowDeltas: true });
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.feed(frame('key', 1000));
  await w.settle();
  if (lastOf(w.posted, 'rendered')) ok('the first painted frame is announced so the page can clear its watchdog');
  else fail('no rendered message after the first keyframe');

  // Enough frames to put the count over the cap, with the clock standing still: a decoder that
  // has produced an output this instant is keeping up, however many pictures it is holding.
  for (let i = 1; i <= 8; i++) {
    w.feed(frame('delta', 1000 + i * 1500));
    await w.settle();
  }
  if (!lastOf(w.posted, 'backlog')) ok('a pipelined decoder that is still producing outputs is not called a backlog');
  else fail('the in-flight count alone tripped the backlog path, which is what a healthy hardware decoder looks like');

  // Now the decoder has stopped: frames keep arriving at 60 fps and nothing comes back out for
  // longer than the stall window. (Time with no input at all is not evidence of a stall: see
  // case 16.)
  for (let i = 9; i <= 16; i++) {
    w.advance(16);
    w.feed(frame('delta', 1000 + i * 1500));
    await w.settle();
  }
  const backlog = lastOf(w.posted, 'backlog');
  if (!backlog) fail('a decoder that accepts and never outputs did not trip the backlog path');
  else if (backlog.queue !== 0) fail('the backlog was reported off decodeQueueSize, which is ' + backlog.queue);
  else ok('backpressure trips on decodes without outputs while decodeQueueSize reads zero');

  w.send({ type: 'videostats' });
  const st = lastOf(w.posted, 'videostats').stats;
  if (st.framesDropped > 0) ok('the dropped frames are counted, not silently discarded');
  else fail('framesDropped stayed at 0 through a backlog');
  if (st.waitingForKey) ok('a backlog resynchronises to the next keyframe rather than queueing');
  else fail('the backlog did not ask for a resync');
}

// 5. The keyframe tiers are reported honestly. `sendKeyFrameRequest` existing is not the same
//    as it working, and the out-of-band opcode is only usable once the host has said it parses
//    it. A tier that cannot be withdrawn lies.
{
  const w = makeWorker();
  w.attachCanvas();
  w.startPump();               // encodedstream: no transformer, so no sendKeyFrameRequest
  await w.settle();
  const start = lastOf(w.posted, 'transformstart');
  if (!start) fail('no transformstart message');
  else {
    eq('the encoded-streams transport tier is 2', start.transport, 2);
    eq('and it has no sendKeyFrameRequest', start.hasSendKey, false);
  }
  // With no host capability, asking for a key must not post a requestkey the page would
  // forward to a host that drops the opcode.
  eq('no out-of-band key request before the host advertises it', w.posted.filter((m) => m.type === 'requestkey').length, 0);

  w.send({ type: 'outofbandkey', available: true });
  w.advance(1000);
  w.feed(frame('delta', 5000));   // still waiting for a key -> requests one
  await w.settle();
  if (w.posted.some((m) => m.type === 'requestkey')) ok('once the host advertises the opcode the request goes out');
  else fail('no requestkey after the capability arrived');

  w.send({ type: 'outofbandkey', available: false });
  w.send({ type: 'stats' });
  const stats = lastOf(w.posted, 'stats');
  eq('withdrawing the capability drops the key tier back to none', stats.keyTier, 0);
}

// 6. A stream with no keyframe gives up loudly rather than freezing.
{
  const w = makeWorker({ startClock: 0 });
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.setClock(9000); // past KEY_WAIT_GIVE_UP_MS (8000)
  w.feed(frame('delta', 1000));
  await w.settle();
  const gave = lastOf(w.posted, 'nokeyframe');
  if (!gave) fail('a stream with no keyframe for 9 s did not report nokeyframe');
  else if (gave.waitedMs < 8000) fail('nokeyframe reported only ' + gave.waitedMs + ' ms of waiting');
  else ok('a stream with no keyframe for 9 s reports nokeyframe with the real wait');
}

// 7. The codec hint picks the configuration, and a malformed one is ignored.
{
  const w = makeWorker();
  w.attachCanvas();
  w.send({ type: 'codechint', profileLevelId: '4d001f', width: 1280, height: 720 });
  w.startPump();
  await w.settle();
  w.feed(frame('key', 1000));
  await w.settle();
  const cfg = w.configuredWith;
  if (!cfg) fail('the decoder was never configured');
  else {
    eq('the negotiated profile-level-id becomes the codec string', cfg.codec, 'avc1.4D001F');
    eq('the coded size hint is offered', cfg.codedWidth, 1280);
    if (cfg.optimizeForLatency) ok('optimizeForLatency is part of the configuration that was probed');
    else fail('optimizeForLatency was dropped between probe and configure');
  }
}
{
  const w = makeWorker();
  w.attachCanvas();
  w.send({ type: 'codechint', profileLevelId: 'not-hex', width: 0, height: 0 });
  w.startPump();
  await w.settle();
  w.feed(frame('key', 1000));
  await w.settle();
  const cfg = w.configuredWith;
  if (cfg && cfg.codec.startsWith('avc1.') && cfg.codec !== 'avc1.NOT-HEX') ok('a malformed profile-level-id falls back to the candidate list');
  else fail('a malformed hint reached the decoder as ' + (cfg && cfg.codec));
  if (cfg && cfg.codedWidth === undefined) ok('no size hint is offered when the page did not supply one');
  else fail('a size hint appeared from nowhere');
}

// 8. A size hint that is refused is retried without it, and named. The hinted size is what
//    the client asked for, not what the host sends: the host snaps DPR, rounds, clamps and
//    applies a per-device scale. Failing over it would demote a session that decodes fine.
{
  const w = makeWorker({ rejectSized: true });
  w.attachCanvas();
  w.send({ type: 'codechint', profileLevelId: '42e01f', width: 3840, height: 2160 });
  w.startPump();
  await w.settle();
  w.feed(frame('key', 1000));
  await w.settle();
  const cfg = w.configuredWith;
  if (cfg && cfg.codedWidth === undefined) ok('a refused size hint is dropped rather than failing the session');
  else fail('the session kept a size hint every candidate refused');
  w.send({ type: 'stats' });
  w.send({ type: 'videostats' });
  const vs = lastOf(w.posted, 'videostats');
  if (vs && vs.stats.sizeHintRejected) ok('the retry is reported, so "we asked the wrong question" is distinguishable from "no H.264"');
  else fail('sizeHintRejected was not reported');
}

// 9. The stats the host parses are all present. index.html folds these into the OP::STATS
//    frame, and a renamed field becomes a zero on the host with nothing failing.
{
  const w = makeWorker();
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.feed(frame('key', 1000));
  await w.settle();
  w.send({ type: 'videostats' });
  const vs = lastOf(w.posted, 'videostats');
  if (!vs) fail('no videostats reply');
  else {
    const need = ['arrivalToPaint', 'decode', 'hostToPaint', 'framesIn', 'framesDecoded',
      'framesRendered', 'framesUnpainted', 'framesDropped', 'decodeErrors', 'keyRequests',
      'clockValid', 'decoderHardware', 'transportTier', 'renderTier', 'keyTier', 'suppressed',
      'displayWidth', 'displayHeight'];
    const missing = need.filter((k) => !(k in vs.stats));
    if (missing.length) fail('videostats is missing ' + missing.join(', '));
    else ok('videostats carries every field the STATS frame is built from');
    eq('the canvas 2D render tier is 1', vs.stats.renderTier, 1);
    if (vs.stats.arrivalToPaint && typeof vs.stats.arrivalToPaint.p50 === 'number') ok('arrival->paint reports percentiles, not a mean');
    else fail('arrivalToPaint has no p50');
  }
}

// 10. A canvas with no 2D context does not claim a render tier. Claiming it would count every
//     frame as rendered while nothing is drawn: the watchdog clears, the stall detector sees
//     progress, and the session sits live on a black canvas.
{
  const w = makeWorker();
  w.send({ type: 'canvas', canvas: { getContext: () => null } });
  const err = lastOf(w.posted, 'configerror');
  if (err && /2d context/.test(err.message)) ok('a canvas with no 2D context reports a config error');
  else fail('a contextless canvas was accepted silently');
  w.send({ type: 'videostats' });
  eq('and claims no render tier', lastOf(w.posted, 'videostats').stats.renderTier, 0);
}

// 11. The stage percentiles sort numerically. A sort that compared them as strings would put
//     100 before 9 and report a p99 below the p50, and nothing else would notice.
{
  const w = makeWorker();
  const sw = new w.ctx.StatWindow();
  for (const v of [9, 100, 2.5, 10, 0, 33.25, 1000, 7]) sw.push(v);
  sw.push(-1);
  sw.push(NaN);
  const p = sw.pct();
  eq('a stage window keeps only finite non-negative samples', p.n, 8);
  eq('its max is the largest sample', p.max, 1000);
  eq('its p50 is the fifth smallest of eight', p.p50, 10);
  eq('its p90 is the eighth', p.p90, 1000);
  eq('an empty window reports nothing rather than zero', new w.ctx.StatWindow().pct(), null);
}

// 12. Keyframes, and only keyframes, go back to the receiver. Its own decoder never sees a
//     frame otherwise and asks for a keyframe every ~200 ms for the whole session. One keyframe
//     gives it something decodable and moves it onto its 3 s timeout; handing it every delta
//     would decode every frame twice.
for (const via of ['script transform', 'createEncodedStreams']) {
  const w = makeWorker();
  w.attachCanvas();
  const rw = w.receiverWritable();
  if (via === 'script transform') w.startScriptTransform(rw); else w.startPump(rw);
  await w.settle();
  w.feed(frame('delta', 1000));
  w.feed(frame('key', 4000));
  w.feed(frame('delta', 7000));
  w.feed(frame('delta', 10000));
  w.feed(frame('key', 13000));
  await w.settle();
  eq(via + ': frames handed back to the receiver', rw.written.join(' '), 'key@4000 key@13000');
  eq(via + ': our own decoder still got every frame from the first keyframe on', w.decoded.length, 4);
  w.send({ type: 'videostats' });
  eq(via + ': keyframesForwarded in the stats', lastOf(w.posted, 'videostats').stats.keyframesForwarded, 2);
}
{
  // a page that sends no writable (an older page, or a browser that cannot transfer one)
  // still decodes
  const w = makeWorker();
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.feed(frame('key', 4000));
  w.feed(frame('delta', 7000));
  await w.settle();
  eq('no writable: both frames still decoded', w.decoded.length, 2);
}

// 13. A keyframe still in the decoder is not a stalled decoder. A 1-2 MB IDR takes longer than
//     BACKLOG_STALL_MS to decode while the deltas that depend on it keep arriving. Calling that
//     a backlog drops those deltas and asks for another keyframe just as large: measured, 20-49
//     IDRs in 20 s with no packet lost.
const stats = (w) => { w.send({ type: 'videostats' }); return lastOf(w.posted, 'videostats').stats; };
{
  const w = makeWorker();
  w.attachCanvas();
  w.startScriptTransform(w.receiverWritable());
  await w.settle();
  w.advance(1000); // clear of the opening request's KEY_REQUEST_MIN_INTERVAL_MS
  w.feed(frame('key', 1000));
  w.feed(frame('delta', 2500));
  await w.settle();
  if (!lastOf(w.posted, 'rendered')) fail('setup: nothing painted before the big keyframe');
  const before = stats(w);
  const decodedBefore = w.decoded.length;

  // The big keyframe follows straight on from a healthy output, as on the wire.
  w.holdOutputs(true);
  w.feed(frame('key', 100000));
  await w.settle();
  // 12 deltas over 150 ms while the keyframe has produced nothing.
  for (let i = 1; i <= 12; i++) {
    w.advance(12.5);
    w.feed(frame('delta', 100000 + i * 1500));
    await w.settle();
  }
  const during = stats(w);
  eq('keyframe in decode: keyframe requests made', during.keyRequests - before.keyRequests, 0);
  eq('keyframe in decode: deltas dropped', during.framesDropped - before.framesDropped, 0);
  eq('keyframe in decode: no backlog reported', w.posted.filter((m) => m.type === 'backlog').length, 0);
  eq('keyframe in decode: the keyframe and all 12 deltas were submitted', w.decoded.length - decodedBefore, 13);
  if (during.keyDecodeGraceHits > 0) ok('the grace is counted (keyDecodeGraceHits = ' + during.keyDecodeGraceHits + ')');
  else fail('keyDecodeGraceHits stayed 0 while the backlog test was held off');
  if (!during.waitingForKey) ok('the stream is not resynchronising to another keyframe');
  else fail('a keyframe in decode put the stream back into waiting for a keyframe');

  // The keyframe comes out 150 ms late and everything behind it follows.
  w.holdOutputs(false);
  const paintedBefore = w.painted.length;
  w.releaseOutputs();
  await w.settle();
  eq('after the keyframe: frames painted (key + 12 deltas)', w.painted.length - paintedBefore, 13);
  w.advance(16);
  w.feed(frame('delta', 100000 + 13 * 1500));
  await w.settle();
  const after = stats(w);
  eq('after the keyframe: still no keyframe request', after.keyRequests - before.keyRequests, 0);
  eq('after the keyframe: in-flight count back to zero', after.inFlight, 0);
}

// 14. A real stall with no keyframe pending still trips, once.
{
  const w = makeWorker({ swallowDeltas: true });
  w.attachCanvas();
  w.startScriptTransform(w.receiverWritable());
  await w.settle();
  w.advance(1000); // clear of the opening request's KEY_REQUEST_MIN_INTERVAL_MS
  w.feed(frame('key', 1000));
  await w.settle();
  const before = stats(w);
  for (let i = 1; i <= 8; i++) { w.feed(frame('delta', 1000 + i * 1500)); await w.settle(); }
  // Frames keep arriving at 60 fps with nothing out for > BACKLOG_STALL_MS.
  for (let i = 9; i <= 16; i++) { w.advance(16); w.feed(frame('delta', 1000 + i * 1500)); await w.settle(); }
  const after = stats(w);
  eq('real stall: keyframe requests', after.keyRequests - before.keyRequests, 1);
  eq('real stall: backlog reported', w.posted.filter((m) => m.type === 'backlog').length, 1);
  eq('real stall: grace not applied', after.keyDecodeGraceHits, 0);
}

// 15. The grace is bounded: a decoder that dies on a keyframe is still caught. Frames keep
//     arriving at 60 fps. Inside KEY_DECODE_GRACE_MS nothing is dropped or requested; past it
//     the stale count is resynchronised, the stall clock restarts from that resync, and the
//     backlog test trips.
{
  const w = makeWorker();
  w.attachCanvas();
  w.startScriptTransform(w.receiverWritable());
  await w.settle();
  w.advance(1000);
  w.feed(frame('key', 1000));
  w.feed(frame('delta', 2500));
  await w.settle();
  const before = stats(w);
  w.holdOutputs(true);
  w.feed(frame('key', 100000));
  await w.settle();
  for (let i = 1; i <= 8; i++) { w.advance(12.5); w.feed(frame('delta', 100000 + i * 1500)); await w.settle(); }
  const mid = stats(w);
  eq('dead keyframe, inside the grace: no request yet', mid.keyRequests - before.keyRequests, 0);
  if (mid.inFlight > 6) ok('dead keyframe, inside the grace: the in-flight count is left alone (' + mid.inFlight + ')');
  else fail('the in-flight count was reset while a keyframe was decoding');
  // Steady 60 fps until the grace has run out, then until the backlog test fires.
  let i = 9;
  // The big keyframe went in at 11000 ms; KEY_DECODE_GRACE_MS ends at 12000.
  while (w.ctx.performance.now() + 16 <= 11000 + 1000) { w.advance(16); w.feed(frame('delta', 100000 + i++ * 1500)); await w.settle(); }
  const atExpiry = stats(w);
  eq('dead keyframe, still inside/at the grace edge: no request yet', atExpiry.keyRequests - before.keyRequests, 0);
  let n = 0;
  while (n < 20 && stats(w).keyRequests === atExpiry.keyRequests) { w.advance(16); w.feed(frame('delta', 100000 + i++ * 1500)); await w.settle(); n++; }
  const after = stats(w);
  eq('dead keyframe, past the grace: one keyframe request', after.keyRequests - before.keyRequests, 1);
  if (n <= 12) ok('dead keyframe, past the grace: caught ' + n + ' frames (' + n * 16 + ' ms) after the grace ran out');
  else fail('a decoder dead on a keyframe took ' + n + ' frames past the grace to catch');
  if (after.waitingForKey) ok('dead keyframe, past the grace: resynchronising to the next keyframe');
  else fail('not resynchronising after the backlog trip');
}

// 16. An arrival gap followed by a clump is not a stalled decoder. The stall clock must not run
//     from the last output while nothing is arriving: 150 ms with no input (a Wi-Fi hiccup),
//     then 12 deltas inside 10 ms, puts `now - lastOutput` past 66 ms before the decoder has
//     been given anything.
async function gapThenClump(w) {
  w.attachCanvas();
  w.startScriptTransform(w.receiverWritable());
  await w.settle();
  w.advance(1000);
  w.holdOutputs(true);
  // A healthy pipelined decoder: every output lands one frame period after its input.
  w.feed(frame('key', 1000)); await w.settle();
  let ts = 1000;
  for (let k = 0; k < 30; k++) { w.advance(16); w.releaseOutputs(); ts += 1500; w.feed(frame('delta', ts)); await w.settle(); }
  w.advance(16); w.releaseOutputs(); await w.settle();
  const before = stats(w);
  const paintedBefore = w.painted.length;
  w.advance(150); // nothing arrives
  for (let k = 0; k < 12; k++) { w.advance(0.8); ts += 1500; w.feed(frame('delta', ts)); await w.settle(); }
  w.advance(20); w.releaseOutputs(); await w.settle();
  return { before, after: stats(w), painted: w.painted.length - paintedBefore };
}
{
  const w = makeWorker();
  const r = await gapThenClump(w);
  eq('gap then clump: keyframe requests', r.after.keyRequests - r.before.keyRequests, 0);
  eq('gap then clump: deltas dropped', r.after.framesDropped - r.before.framesDropped, 0);
  eq('gap then clump: all 12 deltas painted', r.painted, 12);
  if (r.after.stallFloorSavesGap > 0) ok('the arrival-gap floor is what held it off (stallFloorSavesGap = ' + r.after.stallFloorSavesGap + ')');
  else fail('stallFloorSavesGap stayed 0: ' + JSON.stringify({ saves: r.after.stallFloorSaves, key: r.after.stallFloorSavesKey, idle: r.after.stallFloorSavesIdle }));
  eq('gap then clump: no keyframe grace involved', r.after.keyDecodeGraceHits, 0);
}

// 17. A decoder that dies on a steady 60 fps stream is still caught. The floor only moves
//     after an arrival gap or from idle, never on a steady stream, so the stall window starts
//     with the decoder's last real work. Also with an arrival gap just before.
for (const withGap of [false, true]) {
  const label = withGap ? 'dead after a gap' : 'dead on a steady stream';
  const w = makeWorker();
  w.attachCanvas();
  w.startScriptTransform(w.receiverWritable());
  await w.settle();
  w.advance(1000);
  w.holdOutputs(true);
  w.feed(frame('key', 1000)); await w.settle();
  let ts = 1000;
  for (let k = 0; k < 30; k++) { w.advance(16); w.releaseOutputs(); ts += 1500; w.feed(frame('delta', ts)); await w.settle(); }
  w.advance(16); w.releaseOutputs(); await w.settle();
  if (withGap) w.advance(150);
  const before = stats(w);
  // From here the decoder accepts and never outputs.
  let n = 0;
  while (n < 30 && stats(w).keyRequests === before.keyRequests) { if (n) w.advance(16); ts += 1500; w.feed(frame('delta', ts)); await w.settle(); n++; }
  const after = stats(w);
  eq(label + ': one keyframe request', after.keyRequests - before.keyRequests, 1);
  if (n <= 9) ok(label + ': caught on frame ' + n + ' (' + (n - 1) * 16 + ' ms after the decoder\'s last work)');
  else fail(label + ': took ' + n + ' frames to catch');
}

// 18. A dead decoder is caught whatever the arrival rate, and a healthy one never is. The gap
//     floor must not be judged against a rate learned from rendered frames: that rate freezes
//     when output stops, and every arrival further apart than it would move the floor.
async function atRate(w, st, ms, frames) {
  for (let k = 0; k < frames; k++) { w.advance(ms); st.ts += Math.round(ms * 90); w.feed(frame('delta', st.ts)); await w.settle(); }
}
async function startHealthy(preMs, preFrames, opts) {
  const w = makeWorker(opts); w.attachCanvas(); w.startScriptTransform(w.receiverWritable()); await w.settle();
  w.advance(1000); w.feed(frame('key', 1000)); await w.settle();
  const st = { ts: 1000 };
  await atRate(w, st, preMs, preFrames);
  return { w, st };
}
for (const [label, preMs, pre, ms, limitMs] of [
  ['dead on a 5 fps keepalive', 200, 20, 200, 1000],
  ['dead at 5 fps straight after 60 fps', 16.7, 60, 200, 1000],
  ['dead after a drop from 60 to 25 fps', 16.7, 60, 40, 500],
  ['dead at 10 fps', 100, 20, 100, 1000],
  ['dead at 60 fps', 16.7, 60, 16.7, 500],
]) {
  const { w, st } = await startHealthy(preMs, pre);
  const before = stats(w).keyRequests;
  w.holdOutputs(true);
  let n = 0;
  while (n < 80 && stats(w).keyRequests === before) { await atRate(w, st, ms, 1); n++; }
  eq(label + ': requests', stats(w).keyRequests - before, 1);
  const took = (n - 1) * ms;
  if (took <= limitMs) ok(label + ': caught ' + took.toFixed(0) + ' ms after the first unanswered frame');
  else fail(label + ': took ' + took.toFixed(0) + ' ms, limit ' + limitMs);
}
for (const [label, preMs, pre, ms, n] of [
  ['healthy 5 fps for 30 s', 200, 5, 200, 150],
  ['healthy 5 fps for 30 s after 60 fps', 16.7, 60, 200, 150],
  ['healthy 25 fps for 30 s after 60 fps', 16.7, 60, 40, 750],
  ['healthy 1 fps for 30 s after 60 fps', 16.7, 60, 1000, 30],
]) {
  const { w, st } = await startHealthy(preMs, pre);
  const b = stats(w);
  await atRate(w, st, ms, n);
  const a = stats(w);
  eq(label + ': requests', a.keyRequests - b.keyRequests, 0);
  eq(label + ': dropped', a.framesDropped - b.framesDropped, 0);
}
{
  // A pipelined decoder that always holds eight pictures and answers every input with one: over
  // the in-flight cap for good, at any rate, and perfectly healthy. It holds them from its first
  // output, as a real one does: how many pictures a decoder keeps is fixed by its configuration.
  const w = makeWorker(); w.attachCanvas(); w.startScriptTransform(w.receiverWritable()); await w.settle();
  w.advance(1000);
  w.holdOutputs(true);
  w.feed(frame('key', 1000)); await w.settle();
  const st = { ts: 1000 };
  await atRate(w, st, 16.7, 8);    // the pipeline fills, then answers one for one
  for (let k = 0; k < 180; k++) { await atRate(w, st, 16.7, 1); w.releaseOutputs(1); await w.settle(); }
  const b = stats(w);
  const resets = w.resets;
  for (const ms of [200, 40, 16.7]) for (let k = 0; k < 60; k++) { await atRate(w, st, ms, 1); w.releaseOutputs(1); await w.settle(); }
  const a = stats(w);
  eq('healthy, eight pictures held at 5/25/60 fps: requests', a.keyRequests - b.keyRequests, 0);
  eq('healthy, eight pictures held at 5/25/60 fps: dropped', a.framesDropped - b.framesDropped, 0);
  eq('healthy, eight pictures held at 5/25/60 fps: decoder resets', w.resets - resets, 0);
  eq('healthy, eight pictures held at 5/25/60 fps: every picture painted', w.painted.length, w.decoded.length - w.heldCount);
}
{
  // A machine that slept with two submits outstanding: the first frames after it wakes reach the
  // pump before the decoder's outputs do.
  const { w, st } = await startHealthy(16.7, 60);
  const b = stats(w);
  w.holdOutputs(true);
  await atRate(w, st, 16.7, 2);
  w.advance(60000);
  await atRate(w, st, 1, 4);
  w.releaseOutputs(); await w.settle();
  eq('wake from sleep: requests', stats(w).keyRequests - b.keyRequests, 0);
}

// 19. A client slower than its stream drops to the newest frame; one that is not is untouched.
// A model of the worker's thread. The host captures at `fps` (or `period(t)`), each frame reaches
// the page `net` ms later (or when `arrive` says) and carries the browser's receive time unless
// `receiveTime` is false. One thread does everything: a frame that has arrived is read before any
// queued decoder output, as Chrome's transform does; an output costs `paintMs` of the thread when
// painted. The decoder finishes pictures in order, `decodeMs` each (`keyDecodeMs` for a keyframe),
// and outputs a picture only once `hold` more have gone in after it, as a pipelined decoder does.
// A reset costs the thread `resetMs`. A keyframe request is answered `keyDecodeDelayMs` later with
// the next capture, but only a request that actually left the page (posted out of band, or a
// sendKeyFrameRequest call), and never with `refuseKeys`. Latency is paint time minus capture.
const ms90 = (ms) => Math.round(ms * 90) >>> 0;
// The worker's own state, for the few cases that must act at an exact moment. A worker without it
// reads as undefined, so the case fails on what it does rather than crashing.
const inner = (w, name) => { try { return vm.runInContext(name, w.ctx); } catch (_) { return undefined; } };
function pctl(xs, q) { if (!xs.length) return null; const a = xs.slice().sort((x, y) => x - y); return a[Math.min(a.length - 1, Math.floor(q * a.length))]; }
function seeded(seed) { return () => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed / 0x7fffffff; }; }
async function simulate(o) {
  const paintMs = o.paintMs === undefined ? 1 : o.paintMs;
  const w = makeWorker({ paintMs, resetMs: o.resetMs === undefined ? 15 : o.resetMs, configureMs: o.configureMs === undefined ? 10 : o.configureMs });
  w.attachCanvas();
  if (o.legacy) w.startPump(); else w.startScriptTransform(w.receiverWritable());
  if (o.outOfBand) w.send({ type: 'outofbandkey', available: true });
  await w.settle();
  w.advance(1000);
  w.holdOutputs(true);
  const now = () => w.ctx.performance.now();
  const start = now();
  const end = start + (o.seconds || 10) * 1000;
  const holdAt = typeof o.hold === 'function' ? o.hold : () => o.hold || 0;
  const periodAt = o.period || (() => 1000 / (o.fps || 60));
  const deltaMs = typeof o.decodeMs === 'function' ? o.decodeMs : () => (o.decodeMs === undefined ? 2 : o.decodeMs);
  const keyMs = o.keyDecodeMs === undefined ? 6 : o.keyDecodeMs;
  const caps = [], arrivals = [], types = [], capOf = new Map();
  let keyAt = start;
  const gen = (i) => {
    while (caps.length <= i) {
      const j = caps.length;
      const c = j ? caps[j - 1] + periodAt(caps[j - 1] - start) : start;
      const type = c >= keyAt ? 'key' : 'delta';
      if (type === 'key') keyAt = Infinity;
      let a = c + (o.net === undefined ? 5 : o.net) + (type === 'key' ? (o.keyNetMs || 0) : 0);
      if (o.arrive) a = start + o.arrive(c - start, a - start, j);
      caps.push(c); types.push(type); arrivals.push(Math.max(a, j ? arrivals[j - 1] : a));
    }
  };
  const done = [];
  let lastDone = 0, k = 0, decodedN = w.decoded.length, asked = 0, resetsSeen = w.resets;
  const keysAt = [];
  const sent = () => w.posted.filter((m) => m.type === 'requestkey').length + w.sendKeyCalls;
  asked = sent();
  for (;;) {
    gen(k);
    const nextA = arrivals[k];
    const nextR = done.length > holdAt(now() - start) ? done[0] : Infinity;
    const readDue = nextA <= now();
    const t = readDue ? now() : Math.min(nextA, nextR);
    if (t > end) break;
    if (t > now()) w.setClock(t);
    let busy = false;
    for (const [f, to] of o.busy || []) if (now() >= start + f && now() < start + to) { w.setClock(start + to); busy = true; }
    if (busy) continue;
    if (readDue || nextA <= nextR) {
      // `atRead` sees the worker just before this frame is read, and may make it a keyframe.
      if (o.atRead && o.atRead(w, caps[k] - start)) types[k] = 'key';
      // The host stamps a keepalive repeat when it leaves the encoder, a real frame at capture.
      const stamp = caps[k] + (o.stampLag ? o.stampLag(caps[k] - start) : 0);
      const ts = ms90(stamp);
      capOf.set(ts, caps[k]);
      if (types[k] === 'key') keysAt.push(caps[k] - start);
      const f = frame(types[k], ts);
      if (o.receiveTime !== false) { const rt = arrivals[k]; f.getMetadata = () => ({ receiveTime: rt }); }
      w.feed(f);
      k++;
      await w.settle();
      // A reset in this step emptied the decoder before the new chunk went in.
      if (w.resets !== resetsSeen) { resetsSeen = w.resets; done.length = 0; lastDone = now(); decodedN = w.decoded.length - w.heldCount; }
      for (; decodedN < w.decoded.length; decodedN++) {
        const cost = w.decoded[decodedN].type === 'key' ? keyMs : deltaMs(now() - start);
        lastDone = Math.max(now(), lastDone) + cost;
        done.push(lastDone);
      }
    } else {
      done.shift();
      if (o.swallow && o.swallow()) w.dropOutputs(1); else w.releaseOutputs(1);
      await w.settle();
    }
    // A reset emptied the decoder.
    if (w.heldCount < done.length) { done.splice(0, done.length - w.heldCount); if (!done.length) lastDone = now(); }
    const s = sent();
    if (s > asked) { asked = s; if (!o.refuseKeys && keyAt === Infinity) keyAt = now() + (o.keyDelayMs || 100); }
  }
  const lat = (from, to) => w.paints.filter((p) => p.at >= start + from && p.at < start + to).map((p) => p.at - capOf.get(p.ts));
  const fps = (from, to) => lat(from, to).length / ((to - from) / 1000);
  const maxGap = (from, to) => { const ps = w.paints.filter((p) => p.at >= start + from && p.at < start + to); let g = 0; for (let i = 1; i < ps.length; i++) g = Math.max(g, ps[i].at - ps[i - 1].at); return g; };
  const paintedOf = (from, to) => w.paints.filter((p) => p.at >= start + from && p.at < start + to).length;
  return { w, start, lat, fps, maxGap, paintedOf, keysAt, s: stats(w), resets: w.resets, demoted: w.posted.some((m) => m.type === 'nokeyframe' || m.type === 'decoderdead') };
}
const p99 = (xs) => pctl(xs, 0.99);
const okIf = (cond, msg) => (cond ? ok(msg) : fail(msg));

// Sessions whose client is not the bottleneck: the catch-up rule must leave each untouched, with
// every decoded frame painted, no reset, and no keyframe beyond the opening one.
function untouched(label, r) {
  eq(label + ': every decoded frame painted', r.w.paints.length, r.w.decoded.length - r.w.heldCount);
  eq(label + ': decoder resets', r.resets, 0);
  eq(label + ': keyframes sent (the opening one only)', r.keysAt.length, 1);
}
{
  const r = await simulate({ seconds: 10 });
  untouched('healthy 60 fps', r);
  okIf(p99(r.lat(1000, 10000)) <= 20, 'healthy 60 fps: p99 ' + p99(r.lat(1000, 10000)).toFixed(1) + ' ms');
}
for (const hold of [0, 1, 2]) {
  // Arrival jitter of 0 to 150 ms (a busy 2.4 GHz link), on decoders that hold 0, 1 and 2 pictures.
  const rnd = seeded(11 + hold);
  const r = await simulate({ seconds: 30, hold, arrive: (c, a) => a + rnd() * 150 });
  untouched('jitter 0-150 ms, decoder holding ' + hold, r);
  okIf(r.maxGap(2000, 30000) < 200, 'jitter 0-150 ms, decoder holding ' + hold + ': longest paint gap ' + r.maxGap(2000, 30000).toFixed(0) + ' ms');
}
for (const hold of [1, 2]) {
  // A still screen between bursts of motion: 60 fps, then the 200 ms keepalive (stamped as it
  // leaves the encoder, 12 ms after capture), then 60 fps, three times over.
  const still = (t) => (t % 6000) >= 2000 && (t % 6000) < 4000;
  const r = await simulate({ seconds: 18, hold, period: (t) => (still(t) ? 200 : 1000 / 60), stampLag: (t) => (still(t) ? 12 : 0) });
  untouched('60 -> 5 -> 60 fps with keepalive stamps, decoder holding ' + hold, r);
  // A held picture waits for the next input by design: at 5 fps that is 200 ms per picture held.
  okIf(r.maxGap(2000, 18000) <= 200 * hold + 50, '60 -> 5 -> 60 fps, holding ' + hold + ': longest paint gap ' + r.maxGap(2000, 18000).toFixed(0) + ' ms (the keepalive allows ' + (200 * hold) + ')');
}
{
  // A still screen at the 5 fps keepalive, with bursts of motion, on a decoder whose depth rises from
  // 0 to 3 pictures at 4 s, mid keepalive (a reconfigure onto another decode path): the depth learned before is
  // too low for a while, and a still-screen update must still be shown when it comes, not skipped.
  const still = (t) => (t % 6000) >= 2000;
  const r = await simulate({ seconds: 18, hold: (t) => (t < 4000 ? 0 : 3), period: (t) => (still(t) ? 200 : 1000 / 60), stampLag: (t) => (still(t) ? 12 : 0) });
  untouched('keepalive, a decoder whose depth rises from 0 to 3', r);
  // The decoder filling three newly held pictures at 5 fps withholds output for four keepalive
  // periods on its own; the rule must add nothing to that.
  okIf(r.maxGap(2000, 18000) <= 4 * 200, 'keepalive, depth rising: longest paint gap ' + r.maxGap(2000, 18000).toFixed(0) + ' ms (filling 3 pictures at 5 fps takes 800)');
}
{
  // A decoder holding 2 that once every 3 s releases everything for 50 ms, at the keepalive rate:
  // the learned depth reads 0 for a window.
  const r = await simulate({ seconds: 15, hold: (t) => ((t % 3000) < 50 ? 0 : 2), period: (t) => (t < 1500 ? 1000 / 60 : 200) });
  untouched('keepalive, a decoder that releases early once every 3 s', r);
  okIf(r.maxGap(2000, 15000) <= 2 * 200 + 50, 'keepalive, releases early: longest paint gap ' + r.maxGap(2000, 15000).toFixed(0) + ' ms (2 held pictures allow 400)');
}
{
  // Every 2 s a large keyframe: 60 ms to decode and 120 ms longer on the wire, the deltas behind it
  // arriving in one clump.
  let last = 0;
  const r = await simulate({ seconds: 12, keyDecodeMs: 60, keyNetMs: 120, atRead: (w, t) => { if (t - last >= 2000) { last = t; return true; } return false; } });
  eq('large keyframes every 2 s: decoder resets', r.resets, 0);
  eq('large keyframes every 2 s: every decoded frame painted', r.w.paints.length, r.w.decoded.length - r.w.heldCount);
}
{
  const r = await simulate({ seconds: 8, decodeMs: 12 });
  untouched('software decode inside the frame period', r);
}
{
  // A decoder that silently discards one picture in forty: the counters of the stall test stay
  // raised, which must not read as a backlog.
  let n = 0;
  const r = await simulate({ seconds: 10, hold: 1, swallow: () => (++n % 40 === 0) });
  eq('a decoder that swallows pictures: decoder resets', r.resets, 0);
  eq('a decoder that swallows pictures: every picture it gave was painted', r.s.framesStale || 0, 0);
}

// Clients slower than their stream. Each of these fails on a worker without the rule.
{
  const r = await simulate({ seconds: 12, paintMs: 25 });
  const l = r.lat(4000, 12000);
  okIf(p99(l) < 250, 'slow paint: latency bounded, p50 ' + pctl(l, 0.5).toFixed(0) + ' p99 ' + p99(l).toFixed(0) + ' ms');
  eq('slow paint: decoder resets (skipping paints is enough)', r.resets, 0);
  okIf(r.fps(4000, 12000) >= 30, 'slow paint: painted ' + r.fps(4000, 12000).toFixed(1) + ' fps (the paint allows 40)');
}
for (const hold of [0, 2]) {
  // A decoder with 40 fps of throughput on a 60 fps stream, for good.
  const r = await simulate({ seconds: 30, hold, decodeMs: 25, keyDecodeMs: 50, outOfBand: true });
  const l = r.lat(5000, 30000);
  const label = 'decoder too slow for good (holding ' + hold + ')';
  okIf(p99(l) < 1500, label + ': latency bounded, p50 ' + pctl(l, 0.5).toFixed(0) + ' p99 ' + p99(l).toFixed(0) + ' ms');
  const gaps = []; for (let i = 2; i < r.keysAt.length; i++) gaps.push(r.keysAt[i] - r.keysAt[i - 1]);
  okIf(gaps.length >= 3 && Math.min(...gaps) >= 1000, label + ': catch-ups at least 1 s apart (' + gaps.map((g) => g.toFixed(0)).join(', ') + ' ms)');
  okIf(gaps.length >= 3 && gaps[gaps.length - 1] >= 1900, label + ': and backing off to 2 s while they keep recurring');
  okIf(r.fps(5000, 30000) >= 25, label + ': painted ' + r.fps(5000, 30000).toFixed(1) + ' fps');
  okIf(!r.demoted, label + ': never gave up on the decoder');
}
{
  // A few seconds of slow decoding, then a decoder that keeps up behind a slow paint. The
  // paint-bound phase must be drained by skipping, not by a keyframe a second.
  const r = await simulate({ seconds: 16, decodeMs: (t) => (t < 4000 ? 30 : 2), paintMs: 0, outOfBand: true });
  const before = r.resets;
  const r2 = await simulate({ seconds: 16, decodeMs: (t) => (t < 4000 ? 30 : 2), outOfBand: true, paintMs: 25 });
  const late = r2.keysAt.filter((t) => t > 6000).length;
  eq('slow decoding, then a slow paint: no catch-up once the decoder keeps up', late, 0);
  okIf(p99(r2.lat(8000, 16000)) < 250, 'slow decoding, then a slow paint: p99 ' + p99(r2.lat(8000, 16000)).toFixed(0) + ' ms after the switch');
  okIf(before >= 1, 'slow decoding alone caught up (' + before + ')');
}
{
  // A decoder too slow, with no way to ask for a keyframe (the legacy transport before the
  // host's capability bit, or against a host without it). Resetting it would freeze the picture
  // for good; the rule may only skip paints.
  const r = await simulate({ seconds: 14, decodeMs: 25, legacy: true, outOfBand: false });
  eq('no way to ask for a keyframe: decoder resets', r.resets, 0);
  okIf(r.maxGap(2000, 14000) < 150, 'no way to ask for a keyframe: the picture keeps moving (longest gap ' + r.maxGap(2000, 14000).toFixed(0) + ' ms)');
  okIf(!r.demoted, 'no way to ask for a keyframe: no give-up');
}
{
  // A host that never answers (or a link that keeps losing the keyframe): the decoder is only
  // emptied once a keyframe is in hand, so the picture keeps moving, late. After one unanswered
  // wait catch-ups stop asking for the rest of the session.
  const r = await simulate({ seconds: 20, decodeMs: 25, outOfBand: true, refuseKeys: true });
  eq('keyframe requests never answered: decoder resets', r.resets, 0);
  okIf(r.maxGap(2000, 20000) < 100, 'keyframe requests never answered: the picture keeps moving (longest gap ' + r.maxGap(2000, 20000).toFixed(0) + ' ms)');
  okIf(r.fps(2000, 20000) >= 35, 'keyframe requests never answered: painted ' + r.fps(2000, 20000).toFixed(1) + ' fps');
  okIf(!r.demoted, 'keyframe requests never answered: no give-up');
  const reqs = r.w.posted.filter((m) => m.type === 'requestkey').length;
  okIf(reqs >= 1 && reqs <= 8, 'keyframe requests never answered: asked within one wait, then stopped (' + reqs + ' requests)');
  eq('keyframe requests never answered: state reports a client that cannot catch up', r.s.lateState, 2);
}
{
  // Frozen for 3 s and thawed with the whole 3 s queued in the transform's input.
  const r = await simulate({ seconds: 7, busy: [[2000, 5000]], decodeMs: 1 });
  const after = r.w.paints.filter((p) => p.at >= r.start + 5000);
  const age = after.length ? after[0].at - after[0].ts / 90 : Infinity;
  okIf(age < 400, 'frozen tab: the first picture after thawing is ' + age.toFixed(0) + ' ms old');
  okIf(p99(r.lat(5400, 7000)) <= 30, 'frozen tab: back in step within 400 ms');
  eq('frozen tab: decoder resets', r.resets, 0);
}
{
  // The same on a browser that reports no receive time: the wait in the transform's input is
  // invisible, so the queue is replayed. Guarded only against making it worse.
  const r = await simulate({ seconds: 7, busy: [[2000, 5000]], decodeMs: 1, receiveTime: false });
  eq('frozen tab, no receive time: decoder resets', r.resets, 0);
  okIf(p99(r.lat(6000, 7000)) <= 30, 'frozen tab, no receive time: in step again within a second');
}
{
  // A starved thread: 25 ms of every 27 taken by other work for 4 s.
  const busy = []; for (let t = 3000; t < 7000; t += 27) busy.push([t, t + 25]);
  const r = await simulate({ seconds: 10, busy, outOfBand: true });
  eq('starved thread: decoder resets', r.resets, 0);
  okIf(p99(r.lat(7500, 10000)) <= 30, 'starved thread, then free: back in step');
}
{
  // A catch-up due just as the host's own keyframe arrives: it goes straight in, no request.
  let forced = false;
  const r = await simulate({ seconds: 8, decodeMs: 25, outOfBand: true, atRead: (w) => { if (!forced && inner(w, 'catchUpDue')) { forced = true; return true; } return false; } });
  okIf(forced, 'keyframe mid-drop: a catch-up met an arriving keyframe');
  okIf(r.s.catchUpRequests < r.s.lateCatchUps, 'keyframe mid-drop: that catch-up sent no request (' + r.s.catchUpRequests + ' requests, ' + r.s.lateCatchUps + ' catch-ups)');
}
{
  // a keyframe wait that begins cancels a catch-up decided before it
  const w = makeWorker();
  okIf(inner(w, 'catchUpDue = true; setWaitingForKey(true); catchUpDue') === false, 'a keyframe wait cancels a pending catch-up');
}
{
  // A catch-up whose request falls inside the 250 ms request spacing is sent once the spacing
  // allows, not lost.
  let primed = false;
  const r = await simulate({ seconds: 6, decodeMs: 25, outOfBand: true, atRead: (w) => { if (!primed && inner(w, 'catchUpDue')) { primed = true; inner(w, 'lastKeyRequestAt = performance.now()'); } return false; } });
  okIf(primed && r.s.catchUpRequests >= 1 && r.keysAt.length >= 2, 'a catch-up request inside the request spacing is still sent (' + r.s.catchUpRequests + ' sent)');
}
{
  // Catch-ups are not keyframe requests: the host reads that counter as a sign of loss.
  const r = await simulate({ seconds: 10, decodeMs: 25, outOfBand: true });
  okIf(r.s.lateCatchUps >= 1, 'catch-ups happened (' + r.s.lateCatchUps + ')');
  eq('catch-ups leave the keyframe-request counter alone', r.s.keyRequests, 1);
}
for (const legacy of [false, true]) {
  const label = legacy ? 'legacy encoded streams' : 'script transform';
  const r = await simulate({ seconds: 8, decodeMs: 25, legacy, outOfBand: true });
  okIf(r.resets >= 1 && p99(r.lat(3000, 8000)) < 1500, label + ', out-of-band key: a slow decoder is caught up (' + r.resets + ')');
  eq(label + ', out-of-band key: requests went out of band', r.w.posted.filter((m) => m.type === 'requestkey').length, r.s.catchUpRequests);
}
{
  const r = await simulate({ seconds: 8, decodeMs: 25, outOfBand: false });
  okIf(r.resets >= 1 && r.w.sendKeyCalls > 1, 'script transform without the out-of-band key: catches up through sendKeyFrameRequest');
}
{
  const r = await simulate({ seconds: 4, paintMs: 25 });
  for (const k of ['framesStale', 'lateCatchUps', 'catchUpRequests', 'lateState', 'lateMs']) {
    okIf(typeof r.s[k] === 'number', 'stats carry ' + k + ' = ' + Math.round(r.s[k]));
  }
}

// A first keyframe after a long wait goes into a fresh configuration. The host may answer
// seconds before its picture exists, and the decoder configured at the transform's start then
// sits idle: the first keyframe reconfigures it, then decodes and paints.
{
  // A decoder whose first configuration takes decodes and answers nothing: only configuring it
  // again in front of the keyframe gets a picture.
  const w = makeWorker({ stallUntilReconfigured: true });
  w.attachCanvas();
  w.startPump();
  await w.settle();
  const before = w.configures;
  w.advance(12000);
  w.feed(frame('key', 1000));
  await w.settle();
  okIf(w.configures === before + 1, 'a first keyframe after 12 s idle configures the decoder again');
  okIf(w.painted.length === 1, 'and paints, where the decoder as first configured would not');
  okIf(w.posted.some((m) => m.type === 'diag' && /configured again/.test(m.text)), 'and says so');
  const w2 = makeWorker();
  w2.attachCanvas();
  w2.startPump();
  await w2.settle();
  const before2 = w2.configures;
  w2.advance(1000);
  w2.feed(frame('key', 1000));
  await w2.settle();
  okIf(w2.configures === before2 && w2.painted.length === 1, 'a first keyframe soon after the configure is decoded as it is');
}

// A decoder that never starts is reported once, with its counters. Frames arrive and the
// configuration never completes: once the bound passes, the page is told and a line carries the
// counters. Decodes queued behind an initialisation that never finishes are the other way it
// shows, and the one-shot line after the first keyframe says which.
const reported = (w) => w.posted.filter((m) => m.type === 'decodernotstarting');
const restarts = (w) => w.posted.filter((m) => m.type === 'diag' && /^decoder restart, /.test(m.text));
{
  // A configuration that never completes, with frames arriving: reported at 5 s from its own
  // start, not before, and demoted directly (a restart would wait on the same configure).
  const w = makeWorker({ supportCheckNeverAnswers: true });
  w.attachCanvas();
  w.startPump();
  w.feed(frame('key', 1000));
  await w.settle();
  await w.tick(4900);
  okIf(reported(w).length === 0, 'a configuration in progress is not reported at 4.9 s');
  await w.tick(100);
  const told = reported(w);
  okIf(told.length === 1 && /configuration still pending/.test(told[0].reason), 'a configuration that never completes is reported at 5 s');
  okIf(restarts(w).length === 0, 'with no restart first');
  const line = w.posted.find((m) => m.type === 'diag' && /did not start/.test(m.text));
  okIf(!!line && /configuring=true/.test(line.text) && /configuringMs=5000/.test(line.text), 'and a line saying a configure is in flight, and for how long');
  await w.tick(20000);
  okIf(reported(w).length === 1, 'reported once');
}
{
  // The same configuration already past its bound before any frame: reported when one arrives.
  const w = makeWorker({ supportCheckNeverAnswers: true });
  w.attachCanvas();
  w.startPump();
  await w.settle();
  await w.tick(8000);
  okIf(reported(w).length === 0, 'no report while nothing arrives');
  w.feed(frame('key', 1000));
  await w.settle();
  okIf(reported(w).length === 1, 'reported as soon as frames arrive past the bound');
}
for (const ms of [1500, 3000]) {
  // A healthy but slow initialisation: neither reported nor restarted, and it paints.
  const w = makeWorker({ supportCheckMs: ms });
  w.attachCanvas();
  w.startPump();
  w.feed(frame('key', 1000));
  await w.settle();
  await w.tick(ms);
  w.feed(frame('delta', 4000));
  await w.settle();
  await w.tick(20000);
  okIf(reported(w).length === 0 && restarts(w).length === 0 && w.painted.length >= 1,
    'an initialisation that finishes at ' + ms / 1000 + ' s is neither reported nor restarted, and paints');
}
{
  // A keyframe that arrives while the configure is still waiting for the page's codec hint.
  const w = makeWorker({ timed: true });
  w.attachCanvas();
  w.startPump();
  w.feed(frame('key', 1000));
  await w.settle();
  okIf(w.painted.length === 0, 'a keyframe during the codec-hint wait waits for the configure');
  await w.tick(600);
  okIf(w.painted.length === 1, 'and paints once the hint wait ends');
  await w.tick(20000);
  okIf(reported(w).length === 0 && restarts(w).length === 0, 'with nothing reported or restarted');
}
{
  // Decodes queued with nothing out, on a decoder a fresh configuration cures: at 5 s the worker
  // restarts it and asks for a keyframe, and that keyframe paints. No demotion.
  const w = makeWorker({ stallUntilReconfigured: true });
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.send({ type: 'outofbandkey', available: true });
  w.feed(frame('key', 1000));
  w.feed(frame('delta', 4000));
  await w.settle();
  await w.tick(1000);
  const once = w.posted.filter((m) => m.type === 'diag' && /one second after the first keyframe/.test(m.text));
  okIf(once.length === 1, 'one line a second after the first keyframe');
  const t = once.length ? once[0].text : '';
  okIf(/queue=2/.test(t) && /keysHandedBack=/.test(t) && /stale=0/.test(t) && /unpainted=0/.test(t) &&
    /droppedBeforeFirstPaint=0/.test(t) && /decodeErrors=0/.test(t) && /suppressed=/.test(t) && /configuring=false/.test(t),
    'carrying the queue, the keyframes handed back, the dropped and failed counts: ' + t);
  await w.tick(3900);
  okIf(restarts(w).length === 0, 'nothing happens at 4.9 s');
  const before = w.configures;
  await w.tick(100);
  okIf(restarts(w).length === 1 && w.configures === before + 1, 'at 5 s the decoder is restarted');
  okIf(reported(w).length === 0, 'and not reported');
  w.feed(frame('key', 7000));
  await w.settle();
  okIf(w.painted.length === 1, 'the keyframe after the restart paints');
  await w.tick(20000);
  okIf(reported(w).length === 0 && restarts(w).length === 1, 'and nothing more happens');
  okIf(w.posted.filter((m) => m.type === 'diag' && /one second after the first keyframe/.test(m.text)).length === 1, 'the one-second line is not repeated for later keyframes');
}
{
  // A decoder that stays silent after the restart too: reported after the fresh decoder's own
  // full bound, never sooner, and the page's restart request is not run a second time.
  const w = makeWorker({ stallAlways: true });
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.send({ type: 'outofbandkey', available: true });
  w.feed(frame('key', 1000));
  await w.settle();
  await w.tick(5000);
  okIf(restarts(w).length === 1 && reported(w).length === 0, 'first silence: a restart, no report');
  const configures = w.configures;
  w.send({ type: 'resetdecoder' });
  await w.settle();
  okIf(w.configures === configures && w.posted.some((m) => m.type === 'diag' && /already restarted/.test(m.text)),
    'a restart asked by the page after it is not run again');
  w.feed(frame('key', 7000));
  await w.settle();
  await w.tick(4900);
  okIf(reported(w).length === 0, 'the fresh decoder is not reported before its own 5 s');
  await w.tick(100);
  const told = reported(w);
  okIf(told.length === 1 && /queued/.test(told[0].reason) && restarts(w).length === 1, 'second silence: reported, after one restart only');
}
{
  // With no way to ask for a keyframe a restart cannot help: the queued decodes are reported.
  const w = makeWorker({ stallAlways: true });
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.feed(frame('key', 1000));
  await w.settle();
  await w.tick(5000);
  okIf(restarts(w).length === 0 && reported(w).length === 1, 'with no keyframe request available it is reported at 5 s, not restarted');
}

{
  // Told it is hidden before anything else (a worker made on a hidden page): its bounds do not
  // run until the page is shown.
  const w = makeWorker({ stallUntilReconfigured: true });
  w.send({ type: 'visibility', hidden: true });
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.send({ type: 'outofbandkey', available: true });
  w.feed(frame('key', 1000));
  await w.settle();
  await w.tick(20000);
  okIf(restarts(w).length === 0 && reported(w).length === 0, 'a worker hidden from its start runs no bound while hidden');
  w.send({ type: 'visibility', hidden: false });
  await w.tick(4900);
  okIf(restarts(w).length === 0, 'nor within 5 s of the page being shown');
  await w.tick(100);
  okIf(restarts(w).length === 1, 'and its queue bound runs a full 5 s from then');
}
{
  const w = makeWorker({ supportCheckNeverAnswers: true });
  w.send({ type: 'visibility', hidden: true });
  w.attachCanvas();
  w.startPump();
  w.feed(frame('key', 1000));
  await w.settle();
  await w.tick(20000);
  okIf(reported(w).length === 0, 'a configuration started on a hidden page is not reported while hidden');
  w.send({ type: 'visibility', hidden: false });
  await w.tick(4900);
  okIf(reported(w).length === 0, 'nor within 5 s of the page being shown');
  await w.tick(100);
  okIf(reported(w).length === 1, 'and is reported 5 s after it');
}
{
  // Hidden: both bounds stop, and start in full again when the page is shown.
  const w = makeWorker({ stallUntilReconfigured: true });
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.send({ type: 'outofbandkey', available: true });
  w.feed(frame('key', 1000));
  await w.settle();
  await w.tick(3000);
  w.send({ type: 'visibility', hidden: true });
  await w.tick(20000);
  okIf(restarts(w).length === 0 && reported(w).length === 0, 'nothing is restarted or reported while the page is hidden');
  w.send({ type: 'visibility', hidden: false });
  await w.tick(4900);
  okIf(restarts(w).length === 0, 'nor within 5 s of being shown');
  await w.tick(100);
  okIf(restarts(w).length === 1, 'the queue bound runs its full 5 s from being shown');
}
{
  const w = makeWorker({ supportCheckNeverAnswers: true });
  w.attachCanvas();
  w.startPump();
  w.feed(frame('key', 1000));
  await w.settle();
  w.send({ type: 'visibility', hidden: true });
  await w.tick(20000);
  w.feed(frame('delta', 4000));
  await w.settle();
  okIf(reported(w).length === 0, 'a pending configuration is not reported while hidden, frames arriving or not');
  w.send({ type: 'visibility', hidden: false });
  await w.tick(4900);
  okIf(reported(w).length === 0, 'nor within 5 s of being shown');
  await w.tick(100);
  okIf(reported(w).length === 1, 'the configuration bound runs its full 5 s from being shown');
}

// A decoder that takes the first keyframe and never answers. The page's watchdog asks for a
// restart: the worker throws the decoder away, configures a new one, waits for a keyframe
// again, and the next keyframe paints.
{
  const w = makeWorker();
  w.attachCanvas();
  w.startPump();
  await w.settle();
  w.holdOutputs(true);
  w.feed(frame('key', 1000));
  w.feed(frame('delta', 4000));
  await w.settle();
  eq('a decoder holding everything paints nothing', w.painted.length, 0);
  // With no way to ask for a keyframe the restart is refused and the page told so.
  w.send({ type: 'resetdecoder' });
  await w.settle();
  okIf(w.posted.some((m) => m.type === 'restartrefused'), 'a restart with no way to ask for a keyframe is refused');
  w.send({ type: 'outofbandkey', available: true });
  const before = w.configures;
  w.send({ type: 'resetdecoder' });
  await w.settle();
  okIf(w.configures > before, 'a restart asked by the page configures a new decoder');
  okIf(w.posted.some((m) => m.type === 'diag' && /restart/.test(m.text)), 'and says so in the page console');
  w.holdOutputs(false);
  w.feed(frame('delta', 7000));
  await w.settle();
  eq('a delta after the restart waits for a keyframe', w.painted.length, 0);
  w.feed(frame('key', 10000));
  await w.settle();
  okIf(w.painted.length >= 1, 'the next keyframe paints');
  okIf(!!lastOf(w.posted, 'rendered'), 'and the page hears the first paint');
}

if (failures) {
  console.log('\n' + failures + ' transform-worker assertion(s) failed');
  process.exitCode = 1;
} else {
  console.log('\nall client transform-worker assertions passed');
}
