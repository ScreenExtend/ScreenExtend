// Runs transform-worker.js — the client's WebCodecs video path — under Node with stub
// WebCodecs classes and a stub encoded-frame stream.
//
// This file is the client half of the A/V sync scheme and the only place glass-to-glass
// latency is measured, and it had no test of any kind. What is asserted here is what a browser
// cannot be relied on to reveal: that the RTP timestamp inverts back to a host capture time
// across the 32-bit wrap (~13.25 h of host uptime), that a delta before the resume keyframe is
// dropped and one after the wrap is not, that backpressure is measured as decodes without
// outputs rather than as `decodeQueueSize`, and that every keyframe tier is honestly reported.
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
  let clock = o.startClock === undefined ? 10_000 : o.startClock;
  let decoderState = 'unconfigured';
  let configuredWith = null;
  const probed = [];
  // The queue the pump reads from; `feed` pushes and the pump drains asynchronously.
  const queue = [];
  let waiting = null;
  let closed = false;

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
    setTimeout: (fn) => setImmediate(fn),
    clearTimeout() {},
    queueMicrotask: (fn) => Promise.resolve().then(fn),
    Promise, Map, Array, Math, Float64Array, Uint8Array, Error, isFinite, String, Object,
    VideoDecoder: class {
      constructor(init) {
        this.init = init;
        this.decodeQueueSize = 0;
        Object.defineProperty(this, 'state', { get: () => decoderState });
      }
      configure(cfg) { configuredWith = cfg; decoderState = 'configured'; }
      decode(chunk) {
        decoded.push({ type: chunk.type, timestamp: chunk.timestamp });
        // A decoder that accepts a chunk and never produces an output is the real failure
        // mode this path guards against: `decodeQueueSize` reads zero the whole time.
        if (o.swallowDeltas && chunk.type !== 'key') return;
        this.init.output(new FakeVideoFrame(chunk.timestamp));
      }
      reset() {}
      close() { decoderState = 'closed'; }
    },
    EncodedVideoChunk: class { constructor(init) { Object.assign(this, init); } },
    VideoFrame: FakeVideoFrame,
  };
  ctx.VideoDecoder.isConfigSupported = async (cfg) => {
    probed.push(cfg);
    if (o.rejectSized && cfg.codedWidth) return { supported: false };
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
    probed,
    get configuredWith() { return configuredWith; },
    setClock(ms) { clock = ms; },
    advance(ms) { clock += ms; },
    send(msg) { ctx.onmessage({ data: msg }); },
    // The canvas the page hands over. `getContext` has to succeed or the worker reports
    // `no 2d context` and never claims a render tier.
    attachCanvas() {
      const canvas = {
        width: 0, height: 0,
        getContext: () => ({ drawImage: (f) => painted.push(f.timestamp) }),
      };
      ctx.onmessage({ data: { type: 'canvas', canvas } });
      return canvas;
    },
    startPump() { ctx.onmessage({ data: { type: 'encodedstream', readable } }); },
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

// ── 1. a delta before the first keyframe is never decoded ────────────────────────────────
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

// ── 2. the RTP timestamp inverts back to a host capture time, across the wrap ────────────
// This is the A/V sync contract: the host packetizes H.264 itself precisely so the RTP
// timestamp carries `host_ns_to_rtp90k(capture_ns)`, and this is the only code that inverts
// it. A wrap mishandled here reads as thirteen hours of display lag.
//
// The avsync post is rate limited to one per 200 ms, so every frame below is fed at least that
// far apart on the stub clock.
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

// ── 3. the resume-keyframe gate survives the wrap too ───────────────────────────────
// `renderFromTs` is the raw stamp of the keyframe the stream resumed from. A plain `>=`
// comparison drops every frame for the rest of the session once the stamp wraps past it —
// silently, with no counter, thirteen hours in.
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

// ── 4. backpressure is a decoder that stopped, not one that is merely pipelined ───────
// Two halves, and the second is the one that was missing. `decodeQueueSize` never leaves 0 in
// the stub, so anything reading it would see a healthy stream while nothing reaches the screen —
// the in-flight count is what has to fire. But the in-flight count *alone* is also what a
// hardware decoder that is keeping up perfectly looks like, because it is pipelined, and the
// response to a backlog is severe: every delta is thrown away until a keyframe arrives, and the
// host gates keyframes to one per 200 ms. So the count is the cheap half of the test and "no
// output for BACKLOG_STALL_MS" is the load-bearing half.
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

  // Now the decoder has genuinely stopped: nothing has come back out for longer than the stall
  // window. This is the state the path exists for.
  w.advance(200);
  for (let i = 9; i <= 12; i++) {
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

// ── 5. the keyframe tiers are reported honestly ──────────────────────────────────────────
// `sendKeyFrameRequest` existing is not the same as it working, and the out-of-band opcode is
// only usable once the host has said it parses it. A tier that cannot be withdrawn lies.
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

// ── 6. a stream with no keyframe gives up loudly rather than freezing ────────────────────
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

// ── 7. the codec hint picks the configuration, and a malformed one is ignored ────────────
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

// ── 8. a size hint that is refused is retried without it, and named ──────────────────────
// The hinted size is what the client asked for, not what the host sends: the host snaps DPR,
// rounds, clamps and applies a per-device scale. Failing over it would demote a session that
// decodes perfectly well.
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

// ── 9. the stats the host parses are all present ─────────────────────────────────────────
// index.html folds these into the OP::STATS frame documented in docs/TELEMETRY.md; a renamed
// field there becomes a zero on the host with nothing failing.
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

// ── 10. a canvas with no 2D context does not claim a render tier ─────────────────────────
// Claiming it meant every frame counted as rendered while nothing was drawn: the watchdog
// cleared, the stall detector saw progress, and the session sat live on a black canvas.
{
  const w = makeWorker();
  w.send({ type: 'canvas', canvas: { getContext: () => null } });
  const err = lastOf(w.posted, 'configerror');
  if (err && /2d context/.test(err.message)) ok('a canvas with no 2D context reports a config error');
  else fail('a contextless canvas was accepted silently');
  w.send({ type: 'videostats' });
  eq('and claims no render tier', lastOf(w.posted, 'videostats').stats.renderTier, 0);
}

// ── 11. the stage percentiles sort numerically ───────────────────────────────────────────
// The windows are sorted once a second on the thread that paints. A sort that compared them as
// strings would put 100 before 9 and report a p99 below the p50, and nothing else would notice.
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

if (failures) {
  console.log('\n' + failures + ' transform-worker assertion(s) failed');
  process.exitCode = 1;
} else {
  console.log('\nall client transform-worker assertions passed');
}
