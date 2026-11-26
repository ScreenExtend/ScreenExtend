'use strict';

// Video fast path: encoded H.264 off the RTP transform, WebCodecs decode, paint on this
// thread. Nothing here may use syntax newer than ES2017: the file is served verbatim to
// whatever browser the client has and one unsupported token kills it whole.

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

// Host-to-local clock offset, measured by the page over PING/PONG (see input.js). At 0, until
// it arrives, the reported delay spans two unrelated epochs and only means something as a
// difference against the audio worker's identically-biased number, which is what A/V sync uses.
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
// a stream with no keyframe for this long will not recover by waiting
const KEY_WAIT_ESCALATE_MS = 1500;
const KEY_WAIT_GIVE_UP_MS = 8000;

// decodeQueueSize counts acceptance, not completion, and can read zero with frames in flight:
// backpressure is the decodes that have not produced an output yet. That count alone is not a
// stall: a pipelined hardware decoder (Windows Media Foundation H.264, even with
// `optimizeForLatency`) holds several pictures while keeping up, and a false trip drops every
// delta until a keyframe: a full-size IDR and a freeze of up to 200 ms, the host's keyframe
// gate (74 IDRs in 30 s at a cap of 2).
const MAX_IN_FLIGHT = 6;
// No output for this long with the count over the cap is a stopped decoder. Two frame periods
// at 30 fps, so it cannot fire on jitter. The clock must not run while nothing arrives:
// stallFloor() moves it to wherever input resumed (after an arrival gap, a submit that found the
// decoder idle, a keyframe submit). Run from the last output alone, the clump behind a large
// IDR tripped it: 20-49 IDRs in 20 s with no packet lost, 7 with the floor.
const BACKLOG_STALL_MS = 66;
// A keyframe in decode is not a stall for this long: nothing can come out until it finishes
// (p99 58-88 ms on a large IDR), so the backlog test and the stale resets stand down. Bounded,
// so a decoder that dies on a keyframe is still caught.
const KEY_DECODE_GRACE_MS = 1000;
let keyDecodeTs = null;        // RTP stamp of the keyframe handed to the decoder, until its output
let keyDecodeSinceMs = 0;      // when it was handed over
let lastKeySubmitMs = 0;       // the stall clock never starts before the latest keyframe submit
let keyDecodeGraceHits = 0;    // frames the backlog test would have dropped during a key decode
// half the stall window: a shorter gap cannot by itself fill it
const ARRIVAL_GAP_MS = 33;
// a gap moves the floor only while the decoder is answering (at most this many submits since
// its last output), or a dead decoder on a sparse stream is never caught
const MAX_UNANSWERED = 2;
// Counting is too slow for a sparse stream (seven frames at 5 fps is 1.4 s), so silence is its
// own test: SILENT_MIN submits and SILENT_MS of arrivals with no output. Each gap counts for at
// most SILENT_GAP_CAP_MS, so a machine waking from suspend is not a dead decoder.
const SILENT_MS = 700;
const SILENT_MIN = 3;
const SILENT_GAP_CAP_MS = 400;
let submitsSinceOutput = 0;
let silentMs = 0;              // arrival time accumulated while a submit was unanswered
let silentTrips = 0;
let lastArrivalMs = 0;
let floorGapMs = 0;            // arrival of the first frame after an arrival gap
let floorIdleMs = 0;           // submit that found the decoder drained by its own outputs, or a count resync
let idleSinceOutput = false;   // the count reached zero through outputs, not through a reset
// backlog trips the floor prevented, counted by which floor was latest
let stallFloorSaves = 0;
let stallFloorSavesKey = 0;
let stallFloorSavesGap = 0;
let stallFloorSavesIdle = 0;
let inFlight = 0;
let backlogStreak = 0;
// The count is only repaid by an output, and a decoder may accept a chunk and silently discard
// it (a delta whose reference was lost, a frame after a corrupt NAL). A few of those would pin
// the count over the cap and drop every delta from then on, so it is resynchronised from two
// signals: an empty accept queue, and no output at all for this long.
const IN_FLIGHT_STALE_MS = 500;
let lastOutputAtMs = 0;

// bounded so a decoder that errors on every frame cannot spin: past this, the page decides
const MAX_DECODER_RESETS = 3;
let decoderResets = 0;

// Falling behind. The stall test only catches a decoder that stops. One that keeps producing
// outputs, slower than frames arrive, drifts seconds behind with every counter healthy (a 25 ms
// paint against 60 fps: capture to paint p50 2.5 s). Only time lost on this machine is acted
// on: a frame the network delivered late is still painted. Per output, before the paint:
//  - `lost`: now minus the arrival of the frame that released this output (a decoder holding H
//    pictures outputs frame i when frame i+H goes in), less the least seen since configure. A
//    fixed floor: a recent minimum would rise with a backlog that never drains.
//  - `ahead`: frames submitted after this one, beyond H. Above zero a newer picture is already
//    decodable, so skipping this paint costs the viewer nothing.
// H is the fewest frames submitted after any output in 2-4 s, learned only in step (behind, a
// backlog reads as depth). A draining backlog is left alone. One stuck for LATE_STUCK_MS:
//  1. Skip the paint of any frame with a newer one ahead. Every frame is still decoded, so the
//     reference chain stays whole.
//  2. If that has not helped within LATE_TRIAL_MS, paint everything again; if the lost time
//     keeps growing past CATCHUP_MS, ask for a keyframe and restart the decoder from it when it
//     arrives. Never empty the decoder before the keyframe is in hand: an unanswered request
//     must cost a late picture, not a frozen one.
const LATE_MS = 50;
const LATE_RESUME_MS = 20;
const LATE_STUCK_MS = 150;
const LATE_TRIAL_MS = 150;
// a frame this far lost with a newer one ahead is not painted even while draining: a tab that
// comes back with seconds queued shows the newest picture, not a replay
const STALE_MS = 250;
const CATCHUP_MS = 120;
const CATCHUP_GROWTH_MS = 30;
// Catch-ups are spaced this far apart, doubling to the max while they recur: a keyframe is the
// largest frame on the wire and the slowest to decode. The real cure is the host lowering the
// frame rate, which the state in the stats frame is there to tell it.
const CATCHUP_MIN_INTERVAL_MS = 1000;
// One keyframe every 2 s caps the cost of a client that can never keep up at half a keyframe a
// second, and its lateness at about 2 s times how far its decoder is short (1 s at two thirds of
// the frame rate) between them.
const CATCHUP_MAX_INTERVAL_MS = 2000;
// a catch-up followed by another within this long did not bring the lateness down
const CATCHUP_RECUR_MS = 10000;
const CATCHUP_KEY_RETRY_MS = 500;
// a request unanswered for this long turns catch-ups off for the session
const CATCHUP_KEY_WAIT_MS = 2500;
// "ahead" also needs the next frame captured within this of the current one (RTP stamps): at a
// 5 fps keepalive the next picture is 200 ms away and skipping to it only shows the change late
const SKIP_NEIGHBOUR_MS = 50;
const LATE_WINDOW_MS = 2000;
// Reported in the stats frame: 0 in step, 1 skipping stale paints, 2 behind with skipping no help
// (painting everything, waiting to catch up or unable to), 3 waiting for a catch-up keyframe
// (still painting everything).
const LATE = { inStep: 0, skipping: 1, behind: 2, catchingUp: 3 };
let lateState = LATE.inStep;
// Arrival time per submitted frame, by submission order. A ring, so it never reads empty for a
// backlog larger than `pending`.
const ARRIVAL_RING = 4096;
const arrivalTs = new Uint32Array(ARRIVAL_RING);
const arrivalAt = new Float64Array(ARRIVAL_RING);
let submitSeq = 0;
let outputSeq = 0;
let holdMinCur = Infinity;
let holdMinPrev = Infinity;
let lostFloor = Infinity;
let lateWindowAtMs = 0;
let lateWindows = 0;
let behindSinceMs = 0;
let behindBest = 0;
let behindBestAtMs = 0;
let trialExcess = 0;
let catchUpDue = false;
let catchUpArmed = false;
let catchUpArmedAtMs = 0;
let catchUpsOff = false;
let holdKnown = -1;
let catchUpKeyPending = false;
let lastCatchUpMs = -Infinity;
let catchUpIntervalMs = CATCHUP_MIN_INTERVAL_MS;
let activeConfig = null;
let lateMs = 0;
let latePeakMs = 0;
let lateCatchUps = 0;
let catchUpRequests = 0;
let framesStale = 0;

let estimatedFps = 60;
let lastRtpTicks = null;
let lastRtpWallMs = null;

// The negotiated profile-level-id and the coded size, posted by the page after the WHEP round
// trip. The pump starts as soon as the transform is installed, before setRemoteDescription, so
// the configure waits for them (bounded) rather than probing avc1.640034 with no coded size.
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

// which mechanisms are live, reported to the host in the stats frame
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

// The receiver's own decoder, kept quiet. The transform consumes every frame, so libwebrtc's
// VideoReceiveStream never gets one and asks for a keyframe after 200 ms without a decodable
// frame: an RTCP PLI every 200-300 ms for the whole session, each answered with an IDR. Handing
// each keyframe back gives it one decodable frame, after which it waits on its 3 s frame
// timeout instead: 3-4 PLIs per 10 s instead of 28-35, for one extra decode every ~3 s. Deltas
// are not handed back: that would silence it entirely but decode every frame twice. The host's
// PLI limiter is still needed for pages that do not do this (the relay serves an older page).
let receiverWriter = null;
let keyframesForwarded = 0;
function attachReceiverWritable(writable) {
  receiverWriter = null;
  if (!writable || typeof writable.getWriter !== 'function') return;
  try { receiverWriter = writable.getWriter(); } catch (_) { note(); }
}
// call after the frame has gone to this worker's decoder: the write may take the frame's data
function forwardKeyframe(encodedFrame) {
  if (!receiverWriter) return;
  try {
    const p = receiverWriter.write(encodedFrame);
    keyframesForwarded++;
    if (p && p.then) p.then(null, note);
  } catch (_) {
    note();
    receiverWriter = null;
  }
}

// Per-frame stage timing, the half the host cannot measure. A sliding window of raw samples,
// not an EMA, because the interesting number is the p99. Timestamps are absolute
// (`timeOrigin + now()`) so they compare with the page's and, given the offset, the host clock.
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
// A typed-array copy sorts numerically with no comparator call: about a quarter of the time of
// a boxed Array with `(x, y) => x - y` (~320 us -> ~77 us for 3 x 512 samples in V8), and this
// runs on the decode thread between two output callbacks.
StatWindow.prototype.pct = function () {
  if (!this.n) return null;
  const a = this.buf.slice(0, this.n);
  a.sort();
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
// outputs that arrived before the first paint and were not painted themselves
let outputsBeforeFirstPaint = 0;
// When the decoder was last configured, and how often a first keyframe found it idle long enough
// to configure it again (see the pump).
const IDLE_RECONFIGURE_MS = 5000;
let configuredAtMs = 0;
let idleReconfigures = 0;
// Set when every candidate was refused with the coded-size hint and accepted without it. The
// hint is the client's request, not the host's stream, so this is "the wrong question was
// asked", not "this device cannot decode H.264".
let sizeHintRejected = false;

function nowAbsMs() { return performance.timeOrigin + performance.now(); }

function notePending(ts, arrivalMs) {
  if (pending.size >= PENDING_MAX) {
    const oldest = pending.keys().next();
    if (!oldest.done) pending.delete(oldest.value);
  }
  pending.set(ts, { arrival: arrivalMs, submit: 0 });
}

// Presentation tiers.
// 1. OffscreenCanvas 2D on this thread, the floor: the display compositor picks the newest
//    content at the next vsync without touching the page's main thread.
// 2. VideoTrackGenerator (Safari 18) / MediaStreamTrackGenerator (Chrome 94), which hands
//    decoded frames to a real <video> and may let the compositor take a hardware overlay in
//    fullscreen. Nothing public says the overlay happens, so it is opt-in (`?present=track`).
//
// `desynchronized` is deliberately absent: Chromium implements the low-latency path only on
// HTMLCanvasElement (LowLatencyEnabled lives in html_canvas_element.cc), so on an
// OffscreenCanvas it is inert.
function setupCanvas(off) {
  canvas = off;
  try {
    ctx = canvas.getContext('2d', { alpha: false });
  } catch (_) {
    note();
    ctx = null;
  }
  // never claim the tier without a context: every frame would count as rendered on a black
  // canvas, with the watchdog cleared
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

// A generator that could not be handed to the page must be released: `genWriter` still wins in
// presentFrame, so every frame would go into a writable attached to no sink.
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

// Three outcomes, not two: handed to a writable, drawn, or reached no sink at all. The last
// must not count as rendered, or it clears the no-frame watchdog and blinds the stall detector.
const PRESENT = { handedOff: 1, painted: 2, noSink: 0 };
let lastPostedWidth = 0;

function presentFrame(frame) {
  if (genWriter) {
    try {
      // the writer's promise is the generator's only backpressure signal and is never awaited
      // on the decode path: a slow sink must drop, not queue
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
  // input coordinates are normalised against the picture rect, which needs the intrinsic size:
  // push it as soon as it is known, not on the 1 s stats poll
  if (canvas.width > 0 && canvas.width !== lastPostedWidth) {
    lastPostedWidth = canvas.width;
    self.postMessage({ type: 'size', width: canvas.width, height: canvas.height });
  }
  // Three-argument form: the five-argument destination-rect version applies a scale even
  // when the sizes already match.
  ctx.drawImage(frame, 0, 0);
  return PRESENT.painted;
}

// a line for the page's console, the only one a test harness records
function diag(text) {
  try { self.postMessage({ type: 'diag', text: text }); } catch (_) { note(); }
}

// counters for the diag lines: enough to see a decoder that accepts frames and answers nothing
function decoderSummary() {
  let queue = -1;
  try { if (decoder) queue = decoder.decodeQueueSize; } catch (_) { note(); }
  return 'in=' + framesIn + ' key=' + keyframesIn + ' decoded=' + framesDecoded +
    ' rendered=' + framesRendered + ' inFlight=' + inFlight + ' queue=' + queue +
    ' configured=' + configured + ' configuring=' + (configuring !== null) +
    (configuring !== null ? ' configuringMs=' + Math.round(performance.now() - configureStartedAtMs) : '') +
    ' sinceConfigureMs=' + (configuredAtMs ? Math.round(performance.now() - configuredAtMs) : -1) +
    ' state=' + (decoder ? decoder.state : 'none') + ' waitingForKey=' + waitingForKey +
    ' keysHandedBack=' + keyframesForwarded + ' stale=' + framesStale +
    ' unpainted=' + framesUnpainted + ' droppedBeforeFirstPaint=' + outputsBeforeFirstPaint +
    ' decodeErrors=' + decodeErrors + ' suppressed=' + suppressed + ' resets=' + decoderResets;
}

// How long a decoder gets to start: a configuration from its own start, or decodes queued with
// nothing out from the keyframe that went in. The same 5 s as the page's no-picture clock. A
// demotion costs the session its fast path for good, so these err on the late side; a hardware
// initialisation on a phone, at 4K, can take seconds and then work.
const DECODER_START_BOUND_MS = 5000;
let decoderStartReported = false;
function reportDecoderNotStarting(why) {
  if (decoderStartReported) return;
  decoderStartReported = true;
  diag('decoder did not start: ' + why + ': ' + decoderSummary());
  self.postMessage({ type: 'decodernotstarting', reason: why });
}

// One restart before the first picture, whoever asks for it: this worker when decodes sit queued
// with nothing out, or the page when frames arrive and none paint. `startEpoch` moves with it, so
// a bound armed for the decoder before is not counted against the new one.
let startRestarts = 0;
let startEpoch = 0;
let queueBoundEpoch = -1;
function restartDecoder(why) {
  startRestarts++;
  startEpoch++;
  diag('decoder restart, ' + why + ': ' + decoderSummary());
  resetAndRequestKey();
  ensureConfigured().catch(note);
}

// A hidden page may have its decoding suspended while timers run: both bounds are void while it
// is hidden, and start again in full when it is shown. `boundGen` moves on every hide.
let pageHidden = false;
let boundGen = 0;
let queueBoundHeld = false;
let configureBoundHeld = false;
function setPageHidden(hidden) {
  if (hidden === pageHidden) return;
  pageHidden = hidden;
  if (hidden) {
    boundGen++;
    if (queueBoundEpoch === startEpoch && framesDecoded === 0) queueBoundHeld = true;
    if (configuring) configureBoundHeld = true;
    return;
  }
  if (queueBoundHeld) {
    queueBoundHeld = false;
    startQueueTimer();
  }
  if (configureBoundHeld) {
    configureBoundHeld = false;
    if (configuring) {
      configureStartedAtMs = performance.now();
      startConfigureTimer(configuring);
    }
  }
}

// from a keyframe into a decoder that has produced nothing yet, once per decoder
function armQueueBound() {
  if (framesDecoded > 0 || queueBoundEpoch === startEpoch) return;
  queueBoundEpoch = startEpoch;
  startQueueTimer();
}

function startQueueTimer() {
  if (pageHidden) { queueBoundHeld = true; return; }
  const epoch = startEpoch;
  const gen = boundGen;
  setTimeout(function () {
    if (epoch !== startEpoch || gen !== boundGen || framesDecoded > 0) return;
    let queue = 0;
    try { if (decoder) queue = decoder.decodeQueueSize; } catch (_) { note(); }
    if (queue <= 0) return;
    if (startRestarts === 0 && canAskForKey()) restartDecoder('decodes queued with nothing out');
    else reportDecoderNotStarting('decodes queued behind its initialisation');
  }, DECODER_START_BOUND_MS);
}

// once per session, about a second after the first keyframe reached the worker
function noteFirstKeyframe() {
  setTimeout(function () { diag('one second after the first keyframe: ' + decoderSummary()); }, 1000);
}

self.onmessage = (e) => {
  if (!e.data) return;
  if (e.data.type === 'resetdecoder') {
    // The page saw frames arrive and none paint. A fresh decoder and a keyframe are cheaper than
    // the <video> path for the rest of the session, and are tried once before it, but only where
    // a keyframe can actually be asked for: otherwise emptying the decoder only delays the
    // demotion.
    if (!canAskForKey()) {
      diag('decoder restart refused: no way to ask for a keyframe: ' + decoderSummary());
      self.postMessage({ type: 'restartrefused' });
      return;
    }
    // the one restart already ran; a second gets the page nothing but a later demotion
    if (startRestarts > 0) {
      diag('decoder restart asked by the page, already restarted once: ' + decoderSummary());
      return;
    }
    restartDecoder('asked by the page');
    return;
  }
  if (e.data.type === 'visibility') {
    setPageHidden(!!e.data.hidden);
    return;
  }
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
    // the peak is per poll: the page asks once a second
    latePeakMs = lateMs;
  } else if (e.data.type === 'rendercount') {
    self.postMessage({ type: 'rendercount', n: framesRendered });
  } else if (e.data.type === 'encodedstream') {
    transportTier = TRANSPORT.encodedStreams;
    attachReceiverWritable(e.data.writable);
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
// refreshed, so compare wrap-aware: a plain `ts >= renderFromTs` drops every frame after the
// 32-bit wrap.
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
    idleReconfigures: idleReconfigures,
    framesRendered: framesRendered,
    framesUnpainted: framesUnpainted,
    framesDropped: framesDroppedBacklog,
    framesStale: framesStale,
    lateCatchUps: lateCatchUps,
    catchUpRequests: catchUpRequests,
    lateState: lateState,
    lateMs: latePeakMs,
    keyDecodeGraceHits: keyDecodeGraceHits,
    silentTrips: silentTrips,
    stallFloorSaves: stallFloorSaves,
    stallFloorSavesKey: stallFloorSavesKey,
    stallFloorSavesGap: stallFloorSavesGap,
    stallFloorSavesIdle: stallFloorSavesIdle,
    keyframesIn: keyframesIn,
    keyframesForwarded: keyframesForwarded,
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

// A keyframe decode is pending from its submit until the decoder outputs that stamp or a later
// one (wrap-aware, like `beforeRenderStart`), and is forgotten on any reset. Bounded by
// KEY_DECODE_GRACE_MS so a decoder that dies on a keyframe is still caught.
function keyDecodeInGrace(nowMs) {
  if (keyDecodeTs === null) return false;
  if (nowMs - keyDecodeSinceMs > KEY_DECODE_GRACE_MS) return false;
  return true;
}
function clearKeyDecode() { keyDecodeTs = null; keyDecodeSinceMs = 0; }
function stallFloor() { return Math.max(lastOutputAtMs, lastKeySubmitMs, floorGapMs, floorIdleMs); }
function resetStallFloor() { lastArrivalMs = 0; floorGapMs = 0; floorIdleMs = 0; idleSinceOutput = false; submitsSinceOutput = 0; silentMs = 0; }

function forgetLateness() {
  holdMinCur = Infinity; holdMinPrev = Infinity; lostFloor = Infinity; holdKnown = -1;
  lateWindowAtMs = 0; lateWindows = 0; behindSinceMs = 0; lateMs = 0;
  outputSeq = submitSeq;
  catchUpArmed = false;
}

// After a catch-up's reset the depth is learned again, and the one learned before stands in until
// a window has been seen: same configuration, so the same decoder, and too high is harmless.
function relearnHold() {
  const h = Math.min(holdMinPrev, holdMinCur);
  if (h !== Infinity) holdKnown = h;
  holdMinCur = Infinity; holdMinPrev = Infinity;
  lateWindowAtMs = 0; lateWindows = 0;
}

// when this frame first became known on this machine, recorded as it goes to the decoder
function noteSubmitted(ts, knownAtMs) {
  const i = submitSeq & (ARRIVAL_RING - 1);
  arrivalTs[i] = ts >>> 0;
  arrivalAt[i] = knownAtMs;
  submitSeq++;
  if (submitSeq - outputSeq > ARRIVAL_RING) outputSeq = submitSeq - ARRIVAL_RING;
}

function canAskForKey() {
  if (outOfBandKey) return true;
  return !!(transformer && typeof transformer.sendKeyFrameRequest === 'function' &&
    sendKeyFailures < MAX_SEND_KEY_FAILURES);
}

// Called once per output, before the paint; true means "do not paint this one". On a session
// that keeps up: a ring lookup, a few compares and two subtractions.
function judgeLateness(ts, nowMs) {
  const t = ts >>> 0;
  const mask = ARRIVAL_RING - 1;
  // anything older that never came out was dropped by the decoder
  while (outputSeq < submitSeq && arrivalTs[outputSeq & mask] !== t &&
         ((t - arrivalTs[outputSeq & mask]) | 0) > 0) outputSeq++;
  if (outputSeq >= submitSeq || arrivalTs[outputSeq & mask] !== t) return false;
  const seq = outputSeq++;
  const after = submitSeq - 1 - seq;
  if (nowMs - lateWindowAtMs > LATE_WINDOW_MS) {
    holdMinPrev = holdMinCur;
    holdMinCur = Infinity;
    lateWindowAtMs = nowMs;
    lateWindows++;
  }
  if (lateState === LATE.inStep && after < holdMinCur) holdMinCur = after;
  const learning = lateWindows < 2;
  let hold = Math.min(holdMinPrev, holdMinCur);
  if (hold !== Infinity && !learning) holdKnown = hold;
  else hold = holdKnown >= 0 ? holdKnown : 0;
  const release = seq + hold;
  const lost = release < submitSeq ? nowMs - arrivalAt[release & mask] : 0;
  if (lost < lostFloor) lostFloor = lost;
  const excess = lost - lostFloor;
  lateMs = excess;
  if (excess > latePeakMs) latePeakMs = excess;
  // the first window after a configure is spent learning how many pictures this decoder holds
  if ((learning && holdKnown < 0) || lateState === LATE.catchingUp) return false;
  const newer = after > hold &&
    ((arrivalTs[(seq + 1) & mask] - t) >>> 0) <= SKIP_NEIGHBOUR_MS * RTP_HZ / 1000;

  if (excess <= (lateState === LATE.skipping ? LATE_RESUME_MS : LATE_MS)) {
    behindSinceMs = 0;
    lateState = LATE.inStep;
    return false;
  }
  const stale = newer && excess > STALE_MS && lateState !== LATE.behind;
  if (!behindSinceMs || excess < behindBest) {
    // late, but draining (or only just late): nothing to fix yet
    if (!behindSinceMs) behindSinceMs = nowMs;
    behindBest = excess;
    behindBestAtMs = nowMs;
    return stale || (newer && lateState === LATE.skipping);
  }
  const wait = lateState === LATE.skipping ? LATE_TRIAL_MS : LATE_STUCK_MS;
  if (nowMs - behindBestAtMs < wait) return stale || (newer && lateState === LATE.skipping);

  if (lateState === LATE.inStep) {
    // behind and not catching up on its own: skipping is always tried first
    lateState = LATE.skipping;
    behindBest = excess;
    behindBestAtMs = nowMs;
    trialExcess = excess;
    return newer;
  }
  // Skipping did not help, so the paint is not what is slow: paint everything the decoder
  // gives, and catch up if the lost time keeps growing and a keyframe can be asked for.
  lateState = LATE.behind;
  if (excess > CATCHUP_MS && excess > trialExcess + CATCHUP_GROWTH_MS && !catchUpDue &&
      !catchUpArmed && !catchUpsOff && nowMs - lastCatchUpMs >= catchUpIntervalMs &&
      canAskForKey() && !keyDecodeInGrace(nowMs)) {
    catchUpDue = true;
  }
  return false;
}

// A keyframe is in hand: throw away what the decoder holds and start again from it. Runs in the
// pump, between two reads, never inside an output callback.
function catchUp(nowMs) {
  catchUpDue = false;
  catchUpArmed = false;
  // back again this soon: the last one did not bring the lateness down for long
  if (nowMs - lastCatchUpMs < CATCHUP_RECUR_MS) {
    catchUpIntervalMs = Math.min(catchUpIntervalMs * 2, CATCHUP_MAX_INTERVAL_MS);
  } else {
    catchUpIntervalMs = CATCHUP_MIN_INTERVAL_MS;
  }
  lastCatchUpMs = nowMs;
  lateCatchUps++;
  const discarded = inFlight;
  try {
    decoder.reset();
    decoder.configure(activeConfig);
  } catch (_) {
    note();
    resetAndRequestKey();
    return false;
  }
  framesDroppedBacklog += discarded;
  inFlight = 0;
  lastOutputAtMs = 0;
  clearKeyDecode();
  resetStallFloor();
  pending.clear();
  outputSeq = submitSeq;
  relearnHold();
  behindSinceMs = 0;
  lateState = LATE.inStep;
  return true;
}

// A catch-up's own request: out of band where the host parses it (it skips the host's PLI
// back-off), else sendKeyFrameRequest. Not counted in `keyRequests`, which the host reads as a
// sign of loss. One swallowed by the request spacing is sent on the next read instead.
function requestCatchUpKey() {
  const now = performance.now();
  if (now - lastKeyRequestAt < KEY_REQUEST_MIN_INTERVAL_MS) { catchUpKeyPending = true; return; }
  catchUpKeyPending = false;
  if (!canAskForKey()) { requestKey(true); return; }
  lastKeyRequestAt = now;
  catchUpRequests++;
  if (outOfBandKey) {
    self.postMessage({ type: 'requestkey' });
    return;
  }
  try {
    const p = transformer.sendKeyFrameRequest();
    if (p && p.then) p.then(function () { sendKeyFailures = 0; }, noteSendKeyFailure);
  } catch (_) { noteSendKeyFailure(); }
}

function onDecodedFrame(frame) {
  const beforeFirstPaint = !renderedOnce;
  framesDecoded++;
  if (inFlight > 0) inFlight--;
  if (inFlight === 0) idleSinceOutput = true;
  const nowMs = performance.now();
  lastOutputAtMs = nowMs;
  submitsSinceOutput = 0;
  silentMs = 0;
  let handedOff = false;
  // Read the timestamp before presenting: a track generator's writable takes ownership of
  // the frame, after which touching it is a detached-object error.
  const ts = frame.timestamp;
  if (keyDecodeTs !== null && (((ts >>> 0) - keyDecodeTs) | 0) >= 0) clearKeyDecode();
  try {
    if (!beforeRenderStart(ts)) {
      if (judgeLateness(ts, nowMs)) {
        framesStale++;
        pending.delete(ts);
        return;
      }
      const outcome = presentFrame(frame);
      handedOff = outcome === PRESENT.handedOff;
      if (outcome === PRESENT.noSink) {
        // nothing painted it, so it is not a rendered frame: counting it would clear the
        // page's no-frame watchdog
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
          diag('first frame painted: ' + decoderSummary());
          self.postMessage({ type: 'rendered' });
        }
      }
    }
  } catch (err) {
    // report it: a throw out of the output callback would otherwise stall framesRendered silently
    note();
    self.postMessage({ type: 'painterror', message: String(err) });
  } finally {
    if (beforeFirstPaint && !renderedOnce) outputsBeforeFirstPaint++;
    if (!handedOff) {
      try { frame.close(); } catch (_) { note(); }
    }
  }
}

function makeDecoder() {
  const d = new VideoDecoder({
    output: onDecodedFrame,
    error: (err) => {
      clearKeyDecode();
      decodeErrors++;
      self.postMessage({ type: 'decodeerror', message: String(err) });
      resetAndRequestKey();
    },
  });
  // ondequeue: Chrome 106+, Firefox 130+, Safari 16.4+. It is the browser's "input accepted"
  // edge, used only to clear a stale backlog promptly: what gates submission is the output
  // callback above.
  if ('ondequeue' in d) {
    hasDequeueEvent = true;
    d.ondequeue = function () {
      if (d.decodeQueueSize !== 0) return;
      // Behind a keyframe that is still decoding, an empty accept queue is the decoder busy on
      // the keyframe, not decodes it dropped.
      if (keyDecodeInGrace(performance.now())) return;
      // An empty accept queue with the counter still over the cap can only mean decodes the
      // browser took and never returned an output for.
      if (inFlight > MAX_IN_FLIGHT) inFlight = 0;
      backlogStreak = 0;
    };
  }
  return d;
}

// The coded size is a hint, never a requirement. It is what the client asked for, and the host
// snaps the DPR, rounds the backing size down to even, clamps it and applies a per-device video
// scale, so the hint is routinely larger than the stream. A UA that answers `isConfigSupported`
// honestly about a size it cannot decode would fail every candidate, so `sized` false is the
// same configuration without it.
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

// Memoised while it runs: startPump calls this un-awaited and the pump awaits it again on the
// first frame. Two runs would both assign `decoder` and orphan a configured VideoDecoder (a
// GPU-process decode session on Chromium) with no close().
let configuring = null;
let configureStartedAtMs = 0;
function ensureConfigured() {
  if (configured && decoder && decoder.state !== 'closed') return Promise.resolve(true);
  if (!configuring) {
    const done = function () { configuring = null; };
    const mine = doConfigure();
    configuring = mine;
    configureStartedAtMs = performance.now();
    configuring.then(done, done);
    startConfigureTimer(mine);
  }
  return configuring;
}

// Bounded from the configuration's own start. Reported only once frames are arriving (the pump
// checks again when one does); a restart would get the same unresolved configure back, so this
// one demotes directly.
function startConfigureTimer(mine) {
  if (pageHidden) { configureBoundHeld = true; return; }
  const gen = boundGen;
  setTimeout(function () {
    if (gen !== boundGen || pageHidden) return;
    if (configuring === mine && framesIn > 0) reportDecoderNotStarting('configuration still pending');
  }, DECODER_START_BOUND_MS);
}

async function doConfigure() {
  await waitForCodecHint();
  const candidates = preferredCodec
    ? [preferredCodec].concat(CODEC_CANDIDATES.filter((c) => c !== preferredCodec))
    : CODEC_CANDIDATES.slice();

  // probe exactly the configuration about to be installed: a UA can accept `{codec}` and
  // reject `{codec, optimizeForLatency}`
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
        // the hinted size was what got refused, and that is the client's request, not the
        // host's stream
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
    // telemetry only: whether this device has a hardware decoder for the chosen config. The
    // decoder is still configured with `no-preference`.
    try {
      const hw = await VideoDecoder.isConfigSupported(withAcceleration(chosen, 'prefer-hardware'));
      decoderHardware = !!(hw && hw.supported);
    } catch (_) { note(); }
  }

  decoder = makeDecoder();
  try {
    decoder.configure(chosen);
    activeConfig = chosen;
    diag('decoder configured: ' + chosen.codec + (decoderHardware ? ' (hardware available)' : ''));
  } catch (err) {
    configError = String(err);
    self.postMessage({ type: 'configerror', message: configError, codec: chosen.codec });
    throw err;
  }
  configured = true;
  configuredAtMs = performance.now();
  setWaitingForKey(true);
  inFlight = 0;
  lastOutputAtMs = 0;
  clearKeyDecode();
  resetStallFloor();
  forgetLateness();
  lateState = LATE.inStep;
  catchUpDue = false;
  catchUpIntervalMs = CATCHUP_MIN_INTERVAL_MS;
  return true;
}

function setWaitingForKey(on) {
  // a catch-up decided before this wait would reset the decoder under the keyframe that ends it
  if (on) catchUpDue = false;
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
  clearKeyDecode();
  resetStallFloor();
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

// The legacy createEncodedStreams transform cannot ask for an IDR: it runs instead of the
// reference finder, so nothing that would emit a PLI ever sees a frame. Ask the host over the
// input channel, but only when it has said it parses the opcode; otherwise `none` is the
// honest tier and the page's demotion is the remedy.
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
  attachReceiverWritable(transformer.writable);
  startPump(transformer.readable);
};

function startPump(readable) {
  const reader = readable.getReader();

  // stamp the keyframe wait here, not in the configure: the pump can see frames before the
  // configure has run, and a `waitingSince` of 0 reads as an instant give-up on the first delta
  setWaitingForKey(true);

  // ask for the first IDR now, not behind the configure, which waits for the codec hint: the
  // host should be producing the keyframe meanwhile
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
      const arrivedAtMs = performance.now();
      // the browser's own receive time, where it reports one, also covers the wait in the
      // transform's input, which a slow paint or a frozen thread fills
      let knownAtMs = arrivedAtMs;
      if (typeof encodedFrame.getMetadata === 'function') {
        try {
          const rt = encodedFrame.getMetadata().receiveTime;
          if (rt > 0 && rt <= arrivedAtMs && arrivedAtMs - rt < 60000) knownAtMs = rt;
        } catch (_) { note(); }
      }
      const gapMs = lastArrivalMs ? arrivedAtMs - lastArrivalMs : 0;
      if (gapMs > ARRIVAL_GAP_MS && submitsSinceOutput <= MAX_UNANSWERED) floorGapMs = arrivedAtMs;
      if (keyDecodeInGrace(arrivedAtMs)) silentMs = 0;
      else if (submitsSinceOutput > 0) silentMs += Math.min(gapMs, SILENT_GAP_CAP_MS);
      lastArrivalMs = arrivedAtMs;
      const type = encodedFrame.type === 'key' ? 'key' : 'delta';
      if (type === 'key') {
        keyframesIn++;
        if (keyframesIn === 1) noteFirstKeyframe();
        armQueueBound();
      }

      if (catchUpDue && decoder && configured && !waitingForKey && !keyDecodeInGrace(arrivedAtMs)) {
        catchUpDue = false;
        catchUpArmed = true;
        catchUpArmedAtMs = arrivedAtMs;
        // a keyframe arriving now needs no request
        if (type !== 'key') {
          lateState = LATE.catchingUp;
          requestCatchUpKey();
        }
      }
      if (catchUpArmed && decoder && configured) {
        if (type === 'key') {
          catchUp(arrivedAtMs);
        } else if (arrivedAtMs - catchUpArmedAtMs > CATCHUP_KEY_WAIT_MS) {
          // unanswered: this path does not deliver a keyframe when asked, so stop asking for good
          catchUpArmed = false;
          catchUpsOff = true;
          lateState = LATE.behind;
        } else if (catchUpKeyPending || arrivedAtMs - lastKeyRequestAt >= CATCHUP_KEY_RETRY_MS) {
          requestCatchUpKey();
        }
      }

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
      const pumpNowMs = performance.now();
      const keyBusy = keyDecodeInGrace(pumpNowMs);
      if (inFlight > MAX_IN_FLIGHT && lastOutputAtMs && !keyBusy &&
          pumpNowMs - stallFloor() > IN_FLIGHT_STALE_MS) {
        inFlight = 0;
        backlogStreak = 0;
        // The count was resynchronised, so the work it counts from here started now: without
        // this a decoder that died long ago would be reset every seven frames and never tripped.
        floorIdleMs = pumpNowMs;
      }

      if (renderedOnce && !waitingForKey && decoder && type !== 'key' && !keyBusy &&
          submitsSinceOutput >= SILENT_MIN && silentMs >= SILENT_MS) {
        silentTrips++;
        framesDroppedBacklog++;
        self.postMessage({ type: 'backlog', inFlight: inFlight, queue: decoder.decodeQueueSize });
        setWaitingForKey(true);
        backlogStreak = 0;
        submitsSinceOutput = 0;
        silentMs = 0;
        requestKey(false);
        continue;
      }

      if (renderedOnce && !waitingForKey && decoder && type !== 'key') {
        var outputStalled = lastOutputAtMs !== 0 &&
          (pumpNowMs - stallFloor()) > BACKLOG_STALL_MS;
        if (inFlight > MAX_IN_FLIGHT && !outputStalled && lastOutputAtMs !== 0 &&
            pumpNowMs - lastOutputAtMs > BACKLOG_STALL_MS) {
          // the last-output clock alone would call this a stall: count which floor said otherwise
          stallFloorSaves++;
          const f = Math.max(lastKeySubmitMs, floorGapMs, floorIdleMs);
          if (f === lastKeySubmitMs) stallFloorSavesKey++;
          else if (f === floorGapMs) stallFloorSavesGap++;
          else stallFloorSavesIdle++;
        }
        if (inFlight > MAX_IN_FLIGHT && outputStalled && keyBusy) {
          // the decoder is on a keyframe: submit the delta, it is needed
          keyDecodeGraceHits++;
          backlogStreak = 0;
        } else if (inFlight > MAX_IN_FLIGHT && outputStalled) {
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
          const pending = ensureConfigured();
          // a configuration already past its bound when the first frames arrive
          if (configuring && !pageHidden && !configureBoundHeld &&
              performance.now() - configureStartedAtMs >= DECODER_START_BOUND_MS) {
            reportDecoderNotStarting('configuration still pending');
          }
          await pending;
        }
        if (waitingForKey && type === 'key') {
          // The first keyframe into a decoder configured long before (the host answered before
          // its picture was ready) goes into a fresh configuration. Configuring again in front
          // of a keyframe loses nothing, so a decoder that sat configured and idle for a long
          // time is never the one asked to start from it.
          if (framesDecoded === 0 && decoder && decoder.state === 'configured' && activeConfig &&
              performance.now() - configuredAtMs > IDLE_RECONFIGURE_MS) {
            const idleMs = Math.round(performance.now() - configuredAtMs);
            try {
              decoder.reset();
              decoder.configure(activeConfig);
              configuredAtMs = performance.now();
              idleReconfigures++;
              diag('decoder configured again before its first keyframe, idle ' + idleMs + ' ms');
            } catch (_) { note(); }
          }
          setWaitingForKey(false);
          if (lateState === LATE.catchingUp) lateState = LATE.inStep;
          renderFromTs = encodedFrame.timestamp;
          decoderResets = 0;
        }
        notePending(encodedFrame.timestamp, arrivalMs);
        const rec = pending.get(encodedFrame.timestamp);
        if (rec) rec.submit = nowAbsMs();
        if (inFlight === 0 && idleSinceOutput) floorIdleMs = performance.now();
        idleSinceOutput = false;
        submitsSinceOutput++;
        inFlight++;
        noteSubmitted(encodedFrame.timestamp, knownAtMs);
        // marked before the decode call: a decoder may output synchronously
        if (type === 'key') {
          keyDecodeTs = encodedFrame.timestamp >>> 0;
          keyDecodeSinceMs = performance.now();
          lastKeySubmitMs = keyDecodeSinceMs;
        }
        decoder.decode(
          new EncodedVideoChunk({
            type,
            timestamp: encodedFrame.timestamp,
            data: encodedFrame.data,
          }),
        );
        if (type === 'key') forwardKeyframe(encodedFrame);
      } catch (err) {
        if (inFlight > 0) inFlight--;
        clearKeyDecode();
        decodeErrors++;
        self.postMessage({ type: 'decodeerror', message: String(err) });
        resetAndRequestKey();
      }
    }
  })();
}
