'use strict';

// Video fast path: encoded H.264 off the RTP transform, WebCodecs decode, paint on this
// thread. Nothing here may use syntax newer than ES2017 -- the file is served verbatim to
// whatever browser the client happens to have and one unsupported token kills it whole.

let canvas = null;
let ctx = null;
let generator = null;         // VideoTrackGenerator / MediaStreamTrackGenerator writer
let genWriter = null;
let decoder = null;
let transformer = null;
let configured = false;
let waitingForKey = true;
let waitingSince = 0;
let renderedOnce = false;
let renderFromTs = null;

let framesIn = 0;
let keyframesIn = 0;
let framesDecoded = 0;
let framesRendered = 0;
let framesUnpainted = 0;
let lastKeyRequestAt = 0;
let configError = null;

const RTP_HZ = 90000;
let rtpUnwrapLast = null;
let rtpWraps = 0;
let videoDelayEmaMs = null;
let lastAvsyncPostMs = 0;

// Host-to-local clock offset measured by the page over PING/PONG (see input.js). Until it
// arrives this stays 0, which reproduces the old behaviour exactly: the reported delay is
// then the distance between two unrelated epochs, so it is only meaningful as a *difference*
// against the audio worker's identically-biased number, which is what A/V sync uses. Once the
// offset lands the same figure becomes a real display lag in milliseconds.
let hostOffsetMs = 0;
let clockValid = false;

// Only `avc1.` is safe: WebKit's isSupportedDecoderCodec() tests the literal prefix and
// throws NotSupportedError on `avc3.`.
const CODEC_CANDIDATES = [
  'avc1.640034', 'avc1.640028', 'avc1.64001F',
  'avc1.4D4034', 'avc1.4D401F',
  'avc1.42E034', 'avc1.42E01F', 'avc1.42001F',
];

const KEY_REQUEST_MIN_INTERVAL_MS = 250;
// A stream with no keyframe for this long is not going to recover by waiting.
const KEY_WAIT_ESCALATE_MS = 1500;
const KEY_WAIT_GIVE_UP_MS = 8000;

// decodeQueueSize counts *acceptance*, not completion: it drops as soon as the control
// message is handed toward the codec, so it can read zero with frames still in flight. The
// real backpressure signal is how many decodes have not yet produced an output.
//
// The cap used to be 2, on the reasoning that a live stream wants one frame in flight. That is
// what a decoder that has *fallen behind* looks like; it is also what a hardware decoder looks
// like when it is keeping up perfectly, because it is pipelined — a Windows Media Foundation
// H.264 decoder holds several pictures even with `optimizeForLatency`. Tripping on the count
// alone therefore fired constantly on a healthy session, and the response is severe: every
// delta is thrown away until a keyframe arrives, and the host gates keyframes to one per
// 200 ms, so each false trip is a 200 ms freeze and a full-size IDR. Measured on the Windows
// dev box against a Chrome client, the host was emitting 74 IDRs in 30 s.
//
// So the count is now the cheap half of the test and `BACKLOG_STALL_MS` is the load-bearing
// half: a decoder that is producing outputs is keeping up however many pictures it is holding.
const MAX_IN_FLIGHT = 6;
// No output at all for this long, with the queue over the cap, is a decoder that has stopped
// rather than one that is pipelined. Two frame periods at 30 fps, so it cannot fire on jitter.
const BACKLOG_STALL_MS = 66;
let inFlight = 0;
let backlogStreak = 0;
// The counter is only repaid by an output, and a decoder may accept a chunk and silently
// discard it — a delta whose reference frame was lost, a frame dropped after a corrupt NAL.
// Three of those put the count permanently over the cap, every subsequent delta is dropped and
// the stream collapses into a keyframe-only slideshow that nothing demotes, because keyframes
// still render. So the count is resynchronised from two independent signals: an empty accept
// queue, and no output at all for this long.
const IN_FLIGHT_STALE_MS = 500;
let lastOutputAtMs = 0;

// Bounded so a decoder that errors on every frame cannot spin: after this many resets we
// stop rebuilding and let the page decide.
const MAX_DECODER_RESETS = 3;
let decoderResets = 0;

let estimatedFps = 60;
let lastRtpTicks = null;
let lastRtpWallMs = null;

// The negotiated profile-level-id and the coded size. They are posted by the page after the
// WHEP round trip, but the pump starts as soon as the transform is installed — before
// setRemoteDescription — so without a wait every client probed and configured avc1.640034
// with no coded size at all, which is exactly the configuration isConfigSupported is least
// able to answer honestly about and the thing "probe what you install" was meant to fix.
// Waiting costs nothing: no frame can be decoded before the first keyframe anyway.
let preferredCodec = null;
let hintWidth = 0;
let hintHeight = 0;
const CODEC_HINT_DEADLINE_MS = 600;
let codecHintPending = true;
let codecHintResolve = null;
const codecHintPromise = new Promise(function (resolve) { codecHintResolve = resolve; });
function noteCodecHint() {
  if (!codecHintPending) return;
  codecHintPending = false;
  if (codecHintResolve) codecHintResolve();
}
function waitForCodecHint() {
  if (!codecHintPending) return Promise.resolve();
  return Promise.race([
    codecHintPromise,
    new Promise(function (resolve) { setTimeout(resolve, CODEC_HINT_DEADLINE_MS); }),
  ]);
}

// Which mechanisms are actually live, reported to the host in the stats frame. A degradation
// nobody can see is a degradation nobody fixes.
const TRANSPORT = { none: 0, scriptTransform: 1, encodedStreams: 2 };
const RENDER = { none: 0, canvas2d: 1, trackGenerator: 2 };
const KEYREQ = { none: 0, sendKeyFrameRequest: 1, outOfBand: 2 };
let transportTier = TRANSPORT.none;
let renderTier = RENDER.none;
let keyTier = KEYREQ.none;
let outOfBandKey = false;
let hasDequeueEvent = false;
let suppressed = 0;

function note() { suppressed++; }

// ---------------------------------------------------------------------------
// Per-frame stage timing.
//
// The host measures itself as far as the socket; this is the other half. Stages are kept as a
// sliding window of raw samples rather than an EMA, because the interesting number is the p99,
// and 512 doubles is nothing next to a decoded frame. Timestamps are absolute
// (`timeOrigin + now()`) so they are comparable with the page's and with the host clock once
// the offset is known.
const STATS_WINDOW = 512;
const PENDING_MAX = 128;

function StatWindow() {
  this.buf = new Float64Array(STATS_WINDOW);
  this.n = 0;
  this.i = 0;
}
StatWindow.prototype.push = function (v) {
  if (!(v >= 0) || !isFinite(v)) return;
  this.buf[this.i] = v;
  this.i = (this.i + 1) % STATS_WINDOW;
  if (this.n < STATS_WINDOW) this.n++;
};
StatWindow.prototype.pct = function () {
  if (!this.n) return null;
  const a = Array.prototype.slice.call(this.buf.subarray(0, this.n));
  a.sort(function (x, y) { return x - y; });
  const at = function (q) { return a[Math.min(a.length - 1, Math.floor(q * a.length))]; };
  return { p50: at(0.5), p90: at(0.9), p99: at(0.99), max: a[a.length - 1], n: a.length };
};

const wArrivalToPaint = new StatWindow();
const wDecode = new StatWindow();
const wHostToPaint = new StatWindow();

// Keyed by the encoded chunk timestamp so a decoded frame can be matched back to its arrival.
// Bounded, because a decoder that never emits must not turn into a leak.
const pending = new Map();
let decodeErrors = 0;
let keyRequests = 0;
let framesDroppedBacklog = 0;
let decoderHardware = false;
// Set when every candidate was refused with the coded-size hint and accepted without it:
// the hint was the client's request, not the host's stream. Reported, because it is the
// difference between "this device cannot decode H.264" and "we asked the wrong question".
let sizeHintRejected = false;

function nowAbsMs() { return performance.timeOrigin + performance.now(); }

function notePending(ts, arrivalMs) {
  if (pending.size >= PENDING_MAX) {
    const oldest = pending.keys().next();
    if (!oldest.done) pending.delete(oldest.value);
  }
  pending.set(ts, { arrival: arrivalMs, submit: 0 });
}

// ---------------------------------------------------------------------------
// Presentation tiers.
//
// 1. OffscreenCanvas 2D on this thread (the floor, and already good: DidDraw ->
//    SetNeedsBeginFrame -> the display compositor picks the newest content at the next
//    vsync, without ever touching the page's main thread).
// 2. VideoTrackGenerator / MediaStreamTrackGenerator, which hands decoded frames to a real
//    <video> element and *may* let the compositor take a hardware overlay in fullscreen.
//    Chrome 94 has the old name, Safari 18 the new one, nobody has both, and no public
//    measurement says the overlay actually happens -- so it is feature-detected but opt-in
//    (`?present=track`), not the default. See docs/manual-tests/client-page.md for the A/B.
//
// `desynchronized` is deliberately absent: Chromium implements the low-latency path only on
// HTMLCanvasElement (LowLatencyEnabled lives in html_canvas_element.cc; offscreen_canvas.cc
// overrides none of it), so on an OffscreenCanvas it is inert and only misleads a reader.
function setupCanvas(off) {
  canvas = off;
  try {
    ctx = canvas.getContext('2d', { alpha: false });
  } catch (_) {
    note();
    ctx = null;
  }
  // Claiming the tier without a context meant every frame counted as rendered while nothing
  // was drawn: the watchdog was cleared, the stall detector saw progress, and the session sat
  // live on a black canvas reporting renderTier 1.
  if (!ctx) {
    canvas = null;
    renderTier = RENDER.none;
    configError = 'no 2d context';
    self.postMessage({ type: 'configerror', message: configError });
    return;
  }
  renderTier = RENDER.canvas2d;
  lastPostedWidth = 0;
}

function trackGeneratorCtor() {
  if (typeof VideoTrackGenerator === 'function') return 'video';
  if (typeof MediaStreamTrackGenerator === 'function') return 'legacy';
  return null;
}

function setupTrackGenerator() {
  const kind = trackGeneratorCtor();
  if (!kind) return null;
  try {
    if (kind === 'video') {
      generator = new VideoTrackGenerator();
      genWriter = generator.writable.getWriter();
      renderTier = RENDER.trackGenerator;
      return generator.track;
    }
    generator = new MediaStreamTrackGenerator({ kind: 'video' });
    genWriter = generator.writable.getWriter();
    renderTier = RENDER.trackGenerator;
    return generator;
  } catch (_) {
    note();
    generator = null;
    genWriter = null;
    return null;
  }
}

// A generator that was set up and then could not be handed to the page owns nothing, but
// `genWriter` still wins in presentFrame, so every decoded frame goes into a writable attached
// to no sink: a permanently black canvas that reports itself as live.
function releaseTrackGenerator() {
  if (genWriter) {
    try { genWriter.close().then(null, note); } catch (_) { note(); }
  }
  if (generator) {
    const t = generator.track || generator;
    try { if (t && typeof t.stop === 'function') t.stop(); } catch (_) { note(); }
  }
  generator = null;
  genWriter = null;
}

// Three outcomes, not two: a frame handed to a writable, a frame drawn, and a frame that
// reached no sink at all. The last one used to be indistinguishable from the second, so it
// still counted as rendered — which cleared the no-frame watchdog and blinded the stall
// detector to the very state it exists to catch.
const PRESENT = { handedOff: 1, painted: 2, noSink: 0 };
let lastPostedWidth = 0;

function presentFrame(frame) {
  if (genWriter) {
    try {
      // The writer's promise is the only backpressure signal the generator offers, and we
      // never await it on the decode path: a slow sink must drop, not queue.
      genWriter.write(frame).then(null, note);
      return PRESENT.handedOff; // the writable owns the frame from here
    } catch (_) {
      note();
      return PRESENT.noSink;
    }
  }
  if (!ctx || !canvas) return PRESENT.noSink;
  if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
    if (frame.displayWidth > 0 && frame.displayHeight > 0) {
      canvas.width = frame.displayWidth;
      canvas.height = frame.displayHeight;
    }
  }
  // The picture rect input coordinates are normalised against needs the intrinsic size, and
  // the 1 s stats poll's first tick is a full second after the stream starts — every tap in
  // that window landed off by half the letterbox bar. Push it the moment it is known.
  if (canvas.width > 0 && canvas.width !== lastPostedWidth) {
    lastPostedWidth = canvas.width;
    self.postMessage({ type: 'size', width: canvas.width, height: canvas.height });
  }
  // Three-argument form: the five-argument destination-rect version applies a scale even
  // when the sizes already match.
  ctx.drawImage(frame, 0, 0);
  return PRESENT.painted;
}

self.onmessage = (e) => {
  if (!e.data) return;
  if (e.data.type === 'canvas') {
    releaseTrackGenerator();
    setupCanvas(e.data.canvas);
  } else if (e.data.type === 'trackgen') {
    const track = setupTrackGenerator();
    if (track) {
      try {
        self.postMessage({ type: 'trackgen', track: track }, [track]);
      } catch (_) {
        note();
        releaseTrackGenerator();
        renderTier = RENDER.none;
        self.postMessage({ type: 'trackgen', track: null });
      }
    } else {
      self.postMessage({ type: 'trackgen', track: null });
    }
  } else if (e.data.type === 'codechint') {
    if (e.data.profileLevelId && /^[0-9a-fA-F]{6}$/.test(e.data.profileLevelId)) {
      preferredCodec = 'avc1.' + e.data.profileLevelId.toUpperCase();
    }
    if (e.data.width > 0 && e.data.height > 0) {
      hintWidth = e.data.width | 0;
      hintHeight = e.data.height | 0;
    }
    noteCodecHint();
  } else if (e.data.type === 'outofbandkey') {
    // Revocable, not a latch: the page only learns whether the host parses the opcode once
    // the first PONG arrives, and a tier that cannot be withdrawn is a tier that lies.
    outOfBandKey = !!e.data.available;
    if (outOfBandKey) {
      if (keyTier === KEYREQ.none) keyTier = KEYREQ.outOfBand;
    } else if (keyTier === KEYREQ.outOfBand) {
      keyTier = KEYREQ.none;
    }
  } else if (e.data.type === 'clockoffset') {
    if (typeof e.data.offsetMs === 'number' && isFinite(e.data.offsetMs)) {
      const first = !clockValid;
      hostOffsetMs = e.data.offsetMs;
      clockValid = true;
      if (first) videoDelayEmaMs = null;
    }
  } else if (e.data.type === 'stats') {
    postStats();
  } else if (e.data.type === 'videostats') {
    self.postMessage({ type: 'videostats', stats: collectStats() });
  } else if (e.data.type === 'rendercount') {
    self.postMessage({ type: 'rendercount', n: framesRendered });
  } else if (e.data.type === 'encodedstream') {
    transportTier = TRANSPORT.encodedStreams;
    self.postMessage({ type: 'transformstart', transport: transportTier, hasSendKey: false });
    startPump(e.data.readable);
  }
};

// Absolute local time of the host capture that produced this RTP timestamp, or null when the
// clock offset has not arrived yet. Shares the unwrap state maintained by reportAvSync, which
// runs on the same frame immediately after.
function hostCaptureAbsMsOf(tsTicks) {
  if (!clockValid || rtpUnwrapLast === null) return null;
  const ts = tsTicks >>> 0;
  let wraps = rtpWraps;
  const d = ts - rtpUnwrapLast;
  if (d < -0x80000000) wraps++;
  else if (d > 0x80000000) wraps--;
  const hostMs = ((wraps * 0x100000000) + ts) * 1000 / RTP_HZ;
  return hostMs - hostOffsetMs;
}

// The 32-bit RTP timestamp wraps every ~13.25 h of host uptime. Everything that compares two
// of them has to go through a signed 32-bit difference or the comparison inverts at the wrap.
function advanceRtpUnwrap(tsTicks) {
  const ts = tsTicks >>> 0;
  if (rtpUnwrapLast !== null) {
    const d = ts - rtpUnwrapLast;
    if (d < -0x80000000) rtpWraps++;
    else if (d > 0x80000000) rtpWraps--;
  }
  rtpUnwrapLast = ts;
  return (rtpWraps * 0x100000000) + ts;
}

// `renderFromTs` is the raw stamp of the keyframe the stream resumed from and is never
// refreshed once the stream is running, so a plain `ts >= renderFromTs` dropped every frame
// for the rest of the session after the wrap — silently, with no counter, until the stall
// detector demoted the whole session to <video> thirteen hours in.
function beforeRenderStart(ts) {
  if (renderFromTs === null) return false;
  return (((ts >>> 0) - renderFromTs) | 0) < 0;
}

function reportAvSync(tsTicks) {
  const ts = tsTicks >>> 0;
  const videoHostMs = advanceRtpUnwrap(ts) * 1000 / RTP_HZ;
  const drawAbsMs = performance.timeOrigin + performance.now();
  const delta = drawAbsMs - (videoHostMs - hostOffsetMs);
  if (videoDelayEmaMs === null) {
    videoDelayEmaMs = delta;
  } else {
    // Asymmetric on purpose: a lower sample is closer to the true delay (the higher ones
    // carry queueing), so track downward fast and upward slowly. audio-worker.js uses the
    // same coefficients, because the A/V target is the difference of the two and a
    // mismatched estimator would move it for no reason.
    const a = delta < videoDelayEmaMs ? 0.30 : 0.05;
    videoDelayEmaMs = videoDelayEmaMs * (1 - a) + delta * a;
  }

  const nowMs = performance.now();
  if (lastRtpTicks !== null && lastRtpWallMs !== null) {
    const tickDelta = ((ts - lastRtpTicks) >>> 0);
    const wallDelta = nowMs - lastRtpWallMs;
    if (tickDelta > 0 && wallDelta > 0 && wallDelta < 200) {
      const frameFps = RTP_HZ / tickDelta;
      if (frameFps >= 10 && frameFps <= 240) {
        estimatedFps = estimatedFps * 0.95 + frameFps * 0.05;
      }
    }
  }
  lastRtpTicks = ts;
  lastRtpWallMs = nowMs;

  if (nowMs - lastAvsyncPostMs > 200) {
    lastAvsyncPostMs = nowMs;
    self.postMessage({ type: 'avsync', videoDelayMs: videoDelayEmaMs, clockValid: clockValid });
  }
}

function collectStats() {
  return {
    arrivalToPaint: wArrivalToPaint.pct(),
    decode: wDecode.pct(),
    hostToPaint: wHostToPaint.pct(),
    framesIn: framesIn,
    framesDecoded: framesDecoded,
    framesRendered: framesRendered,
    framesUnpainted: framesUnpainted,
    framesDropped: framesDroppedBacklog,
    decodeErrors: decodeErrors,
    keyRequests: keyRequests,
    clockValid: clockValid,
    decoderHardware: decoderHardware,
    sizeHintRejected: sizeHintRejected,
    configured: configured,
    transportTier: transportTier,
    renderTier: renderTier,
    keyTier: keyTier,
    hasDequeueEvent: hasDequeueEvent,
    suppressed: suppressed,
    inFlight: inFlight,
    waitingForKey: waitingForKey,
    displayWidth: canvas ? canvas.width : 0,
    displayHeight: canvas ? canvas.height : 0,
    queue: decoder ? decoder.decodeQueueSize : 0,
  };
}

function postStats() {
  self.postMessage({
    type: 'stats',
    framesIn, keyframesIn, framesDecoded, framesRendered, framesUnpainted,
    decoderState: decoder ? decoder.state : 'none',
    queue: decoder ? decoder.decodeQueueSize : 0,
    inFlight, transportTier, renderTier, keyTier,
    waitingForKey, configured, configError,
  });
}

function onDecodedFrame(frame) {
  framesDecoded++;
  if (inFlight > 0) inFlight--;
  lastOutputAtMs = performance.now();
  let handedOff = false;
  // Read the timestamp before presenting: a track generator's writable takes ownership of
  // the frame, after which touching it is a detached-object error.
  const ts = frame.timestamp;
  try {
    if (!beforeRenderStart(ts)) {
      const outcome = presentFrame(frame);
      handedOff = outcome === PRESENT.handedOff;
      if (outcome === PRESENT.noSink) {
        // Nothing painted it, so it is not a rendered frame. Counting it cleared the page's
        // no-frame watchdog and hid the stall from the detector that would have recovered it.
        framesUnpainted++;
        advanceRtpUnwrap(ts);
      } else {
        framesRendered++;
        // Both tiers hand the frame to the compositor synchronously here, so this instant is
        // as close to the paint as this thread can observe.
        const paintMs = nowAbsMs();
        const rec = pending.get(ts);
        if (rec) {
          pending.delete(ts);
          wArrivalToPaint.push(paintMs - rec.arrival);
          if (rec.submit) wDecode.push(paintMs - rec.submit);
        }
        if (clockValid) {
          const hostCaptureAbsMs = hostCaptureAbsMsOf(ts);
          if (hostCaptureAbsMs !== null) wHostToPaint.push(paintMs - hostCaptureAbsMs);
        }
        reportAvSync(ts);
        if (!renderedOnce) {
          renderedOnce = true;
          self.postMessage({ type: 'rendered' });
        }
      }
    }
  } catch (err) {
    // A throw out of the output callback used to stall framesRendered silently and let the
    // page's watchdog demote the whole session to <video>.
    note();
    self.postMessage({ type: 'painterror', message: String(err) });
  } finally {
    if (!handedOff) {
      try { frame.close(); } catch (_) { note(); }
    }
  }
}

function makeDecoder() {
  const d = new VideoDecoder({
    output: onDecodedFrame,
    error: (err) => {
      decodeErrors++;
      self.postMessage({ type: 'decodeerror', message: String(err) });
      resetAndRequestKey();
    },
  });
  // Chrome 106+, Firefox 130+, Safari 16.4+. It is the browser's own "input accepted" edge;
  // we use it only to clear a stale backlog promptly, since the completion signal that
  // actually gates submission is the output callback above.
  if ('ondequeue' in d) {
    hasDequeueEvent = true;
    d.ondequeue = function () {
      if (d.decodeQueueSize !== 0) return;
      // An empty accept queue with the counter still over the cap can only mean decodes the
      // browser took and never returned an output for.
      if (inFlight > MAX_IN_FLIGHT) inFlight = 0;
      backlogStreak = 0;
    };
  }
  return d;
}

// The coded size is a *hint*, never a requirement. It is what the client asked the host for,
// and the host does not build that: it snaps the DPR to a step the OS actually exposes, rounds
// the backing size down to an even number, clamps it, and then applies a per-device video scale
// that shrinks the encoded frame again. So the hinted size is routinely larger than the stream
// that arrives — and a UA that answers `isConfigSupported` honestly about a size it cannot
// decode would fail every candidate and demote the whole session to <video> for good.
//
// A real size still lets a UA answer honestly rather than guess, so it is worth offering; it is
// simply never worth failing over. `sized` false is the same configuration without it.
function decoderConfig(codec, sized) {
  const cfg = { codec: codec, optimizeForLatency: true };
  if (sized && hintWidth > 0 && hintHeight > 0) {
    cfg.codedWidth = hintWidth;
    cfg.codedHeight = hintHeight;
  }
  return cfg;
}
function hasSizeHint() {
  return hintWidth > 0 && hintHeight > 0;
}

function withAcceleration(cfg, mode) {
  const out = { codec: cfg.codec, optimizeForLatency: cfg.optimizeForLatency };
  if (cfg.codedWidth) { out.codedWidth = cfg.codedWidth; out.codedHeight = cfg.codedHeight; }
  out.hardwareAcceleration = mode;
  return out;
}

// Memoised while it runs: startPump calls this un-awaited to get the first keyframe request
// out, and the pump awaits it again on the first frame. Both used to run the whole probe loop
// and both used to assign `decoder`, orphaning a configured VideoDecoder — a GPU-process
// decode session on Chromium — with no close() on every session start.
let configuring = null;
function ensureConfigured() {
  if (configured && decoder && decoder.state !== 'closed') return Promise.resolve(true);
  if (!configuring) {
    const done = function () { configuring = null; };
    configuring = doConfigure();
    configuring.then(done, done);
  }
  return configuring;
}

async function doConfigure() {
  await waitForCodecHint();
  const candidates = preferredCodec
    ? [preferredCodec].concat(CODEC_CANDIDATES.filter((c) => c !== preferredCodec))
    : CODEC_CANDIDATES.slice();

  // Probe exactly the configuration we are about to install. The old code probed
  // `{codec}` and configured `{codec, optimizeForLatency}`, so a UA that accepted the first
  // and rejected the second threw where nothing was watching.
  let chosen = decoderConfig(candidates[0], hasSizeHint());
  if (typeof VideoDecoder.isConfigSupported === 'function') {
    let anySupported = false;
    // Two rounds where a size hint exists: the sized configuration first, because that is the
    // one a UA can answer honestly about, then the same candidates unsized. Only failing both
    // is a real "this device cannot decode H.264", which is the one verdict worth demoting on.
    const rounds = hasSizeHint() ? [true, false] : [false];
    for (let r = 0; r < rounds.length && !anySupported; r++) {
      for (const c of candidates) {
        const cfg = decoderConfig(c, rounds[r]);
        try {
          // `no-preference` first, never `prefer-hardware` alone: Chromium's prefer-hardware
          // installs a GPU-only decoder factory with no software fallback, so it reports
          // unsupported on machines that would decode this stream perfectly well.
          const s = await VideoDecoder.isConfigSupported(cfg);
          if (s && s.supported) {
            chosen = (s.config && s.config.codec) ? s.config : cfg;
            anySupported = true;
            break;
          }
        } catch (_) { note(); }
      }
      if (!anySupported && rounds[r]) {
        // Worth naming: it means the hinted size was the thing being refused, and the size is
        // the client's request rather than the host's stream.
        sizeHintRejected = true;
        console.warn('[webcodecs] no configuration accepted ' + hintWidth + 'x' + hintHeight +
          '; retrying without the coded-size hint');
      }
    }
    if (!anySupported) {
      configError = 'no supported H.264 decoder configuration';
      self.postMessage({ type: 'configerror', message: configError, codec: candidates[0] });
      throw new Error(configError);
    }
    // Separately: does this device have a hardware decoder for the chosen config at all?
    // The answer only feeds telemetry -- we still configure with `no-preference`.
    try {
      const hw = await VideoDecoder.isConfigSupported(withAcceleration(chosen, 'prefer-hardware'));
      decoderHardware = !!(hw && hw.supported);
    } catch (_) { note(); }
  }

  decoder = makeDecoder();
  try {
    decoder.configure(chosen);
  } catch (err) {
    configError = String(err);
    self.postMessage({ type: 'configerror', message: configError, codec: chosen.codec });
    throw err;
  }
  configured = true;
  setWaitingForKey(true);
  inFlight = 0;
  lastOutputAtMs = 0;
  return true;
}

function setWaitingForKey(on) {
  if (on && !waitingForKey) waitingSince = performance.now();
  if (on && waitingSince === 0) waitingSince = performance.now();
  if (!on) waitingSince = 0;
  waitingForKey = on;
}

// Flush what the decoder still holds, then rebuild once. Bounded: a decoder erroring on
// every frame must not turn into a rebuild loop that keeps the picture frozen and the CPU
// busy at the same time.
function resetAndRequestKey() {
  configured = false;
  setWaitingForKey(true);
  inFlight = 0;
  lastOutputAtMs = 0;
  const old = decoder;
  decoder = null;
  if (old) {
    try {
      if (old.state !== 'closed') {
        // reset() discards queued work without the "flush after error" throw that close()
        // can raise; close() then releases the codec.
        try { old.reset(); } catch (_) { note(); }
        old.close();
      }
    } catch (_) { note(); }
  }
  decoderResets++;
  if (decoderResets > MAX_DECODER_RESETS) {
    self.postMessage({ type: 'decoderdead', resets: decoderResets });
    return;
  }
  requestKey(true);
}

// The legacy createEncodedStreams transform has no way to ask for an IDR: the transform runs
// instead of the reference finder, so the machinery that would normally emit a PLI never sees
// a frame. Ask the host directly over the input channel instead of freezing — but only when it
// has said it parses the opcode, or `none` is the honest tier and the page's demotion is the
// only remedy left.
function requestKeyOutOfBand() {
  if (outOfBandKey) {
    keyTier = KEYREQ.outOfBand;
    self.postMessage({ type: 'requestkey' });
    return;
  }
  keyTier = KEYREQ.none;
}

// sendKeyFrameRequest() existing is not the same as it working. A throw or a rejection means
// this receiver has no keyframe mechanism on that rung, so drop to the one below instead of
// returning as though the request had gone out; a later success puts it back.
const MAX_SEND_KEY_FAILURES = 2;
let sendKeyFailures = 0;
function noteSendKeyFailure() {
  note();
  sendKeyFailures++;
  if (sendKeyFailures >= MAX_SEND_KEY_FAILURES) requestKeyOutOfBand();
}

function requestKey(force) {
  const now = (typeof performance !== 'undefined' ? performance.now() : Date.now());
  if (!force && now - lastKeyRequestAt < KEY_REQUEST_MIN_INTERVAL_MS) return;
  lastKeyRequestAt = now;
  keyRequests++;
  const hasSendKey = !!(transformer && typeof transformer.sendKeyFrameRequest === 'function');
  if (hasSendKey && sendKeyFailures < MAX_SEND_KEY_FAILURES) {
    keyTier = KEYREQ.sendKeyFrameRequest;
    try {
      const p = transformer.sendKeyFrameRequest();
      if (p && p.then) p.then(function () { sendKeyFailures = 0; }, noteSendKeyFailure);
    } catch (_) { noteSendKeyFailure(); }
    return;
  }
  requestKeyOutOfBand();
}

let firstReadDone = false;

self.onrtctransform = (event) => {
  transformer = event.transformer;
  transportTier = TRANSPORT.scriptTransform;
  const hasSendKey = !!(transformer && typeof transformer.sendKeyFrameRequest === 'function');
  if (hasSendKey) keyTier = KEYREQ.sendKeyFrameRequest;
  self.postMessage({ type: 'transformstart', transport: transportTier, hasSendKey: hasSendKey });
  startPump(transformer.readable);
};

function startPump(readable) {
  const reader = readable.getReader();

  // Stamp the keyframe wait here, not inside the configure. `waitingSince` starts at 0, and
  // the pump can see frames before the configure has run — so the "waited" it computed was
  // performance.now(), i.e. however long the page had been open. On a page that had been up
  // for more than KEY_WAIT_GIVE_UP_MS that reads as an instant give-up on the first delta.
  setWaitingForKey(true);

  // Ask for the first IDR straight away rather than behind the configure: the configure now
  // waits for the codec hint, and the host should be producing the keyframe while it does.
  requestKey(true);
  ensureConfigured().catch(note);

  (async function pump() {
    for (;;) {
      let result;
      try {
        result = await reader.read();
      } catch (_) {
        note();
        break;
      }
      const value = result.value;
      if (result.done) break;
      const encodedFrame = value;

      if (!firstReadDone) {
        firstReadDone = true;
        self.postMessage({ type: 'firstframe', frameType: encodedFrame.type });
      }

      framesIn++;
      const arrivalMs = nowAbsMs();
      const type = encodedFrame.type === 'key' ? 'key' : 'delta';
      if (type === 'key') keyframesIn++;

      if (waitingForKey && type !== 'key') {
        const waited = performance.now() - waitingSince;
        if (waited > KEY_WAIT_GIVE_UP_MS) {
          self.postMessage({ type: 'nokeyframe', waitedMs: waited, keyTier: keyTier });
          waitingSince = performance.now();
        } else {
          requestKey(waited > KEY_WAIT_ESCALATE_MS);
        }
        continue;
      }

      // Never schedule, always resynchronise: if the decoder is behind, the freshest frame
      // is worth more than the queued ones. A single late frame is normal jitter, so only a
      // repeated overrun counts.
      // Second resync, for engines with no `ondequeue` and for one that stops firing it:
      // frames are still arriving and nothing has come back out, so the decodes the counter
      // is holding are gone.
      if (inFlight > MAX_IN_FLIGHT && lastOutputAtMs &&
          performance.now() - lastOutputAtMs > IN_FLIGHT_STALE_MS) {
        inFlight = 0;
        backlogStreak = 0;
      }

      if (renderedOnce && !waitingForKey && decoder && type !== 'key') {
        var outputStalled = lastOutputAtMs !== 0 &&
          (performance.now() - lastOutputAtMs) > BACKLOG_STALL_MS;
        if (inFlight > MAX_IN_FLIGHT && outputStalled) {
          backlogStreak++;
          framesDroppedBacklog++;
          if (backlogStreak >= 2) {
            self.postMessage({ type: 'backlog', inFlight: inFlight, queue: decoder.decodeQueueSize });
            setWaitingForKey(true);
            backlogStreak = 0;
            requestKey(false);
          }
          continue;
        }
        backlogStreak = 0;
      }

      try {
        if (!configured || !decoder || decoder.state === 'closed') {
          if (decoderResets > MAX_DECODER_RESETS) continue;
          await ensureConfigured();
        }
        if (waitingForKey && type === 'key') {
          setWaitingForKey(false);
          renderFromTs = encodedFrame.timestamp;
          decoderResets = 0;
        }
        notePending(encodedFrame.timestamp, arrivalMs);
        const rec = pending.get(encodedFrame.timestamp);
        if (rec) rec.submit = nowAbsMs();
        inFlight++;
        decoder.decode(
          new EncodedVideoChunk({
            type,
            timestamp: encodedFrame.timestamp,
            data: encodedFrame.data,
          }),
        );
      } catch (err) {
        if (inFlight > 0) inFlight--;
        decodeErrors++;
        self.postMessage({ type: 'decodeerror', message: String(err) });
        resetAndRequestKey();
      }
    }
  })();
}
