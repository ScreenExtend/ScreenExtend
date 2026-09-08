let canvas = null;
let ctx = null;
let decoder = null;
let transformer = null;
let configured = false;
let waitingForKey = true;
let sizedCanvas = false;
let renderedOnce = false;
let renderFromTs = 0;

let framesIn = 0;
let keyframesIn = 0;
let framesDecoded = 0;
let framesRendered = 0;
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

const CODEC_CANDIDATES = [
  'avc1.640034', 'avc1.640028', 'avc1.64001F',
  'avc1.4D4034', 'avc1.4D401F',
  'avc1.42E034', 'avc1.42E01F', 'avc1.42001F',
];

const KEY_REQUEST_MIN_INTERVAL_MS = 250;

let estimatedFps = 60;
let lastRtpTicks = null;
let lastRtpWallMs = null;
let backlogCount = 0;

let preferredCodec = null;

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

function nowAbsMs() { return performance.timeOrigin + performance.now(); }

function notePending(ts, arrivalMs) {
  if (pending.size >= PENDING_MAX) {
    const oldest = pending.keys().next();
    if (!oldest.done) pending.delete(oldest.value);
  }
  pending.set(ts, { arrival: arrivalMs, submit: 0 });
}

self.onmessage = (e) => {
  if (e.data && e.data.type === 'canvas') {
    canvas = e.data.canvas;
    ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
  } else if (e.data && e.data.type === 'codechint') {
    if (e.data.profileLevelId && /^[0-9a-fA-F]{6}$/.test(e.data.profileLevelId)) {
      preferredCodec = 'avc1.' + e.data.profileLevelId.toUpperCase();
    }
  } else if (e.data && e.data.type === 'clockoffset') {
    if (typeof e.data.offsetMs === 'number' && isFinite(e.data.offsetMs)) {
      const first = !clockValid;
      hostOffsetMs = e.data.offsetMs;
      clockValid = true;
      if (first) videoDelayEmaMs = null;
    }
  } else if (e.data && e.data.type === 'stats') {
    postStats();
  } else if (e.data && e.data.type === 'videostats') {
    self.postMessage({ type: 'videostats', stats: collectStats() });
  } else if (e.data && e.data.type === 'rendercount') {
    self.postMessage({ type: 'rendercount', n: framesRendered });
  } else if (e.data && e.data.type === 'encodedstream') {
    self.postMessage({ type: 'transformstart', hasReadable: !!e.data.readable, hasSendKey: false });
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

function reportAvSync(tsTicks) {
  const ts = tsTicks >>> 0;
  if (rtpUnwrapLast !== null) {
    const d = ts - rtpUnwrapLast;
    if (d < -0x80000000) rtpWraps++;
    else if (d > 0x80000000) rtpWraps--;
  }
  rtpUnwrapLast = ts;
  const videoHostMs = ((rtpWraps * 0x100000000) + ts) * 1000 / RTP_HZ;
  const drawAbsMs = performance.timeOrigin + performance.now();
  const delta = drawAbsMs - (videoHostMs - hostOffsetMs);
  if (videoDelayEmaMs === null) {
    videoDelayEmaMs = delta;
  } else {
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
    framesDropped: framesDroppedBacklog,
    decodeErrors: decodeErrors,
    keyRequests: keyRequests,
    clockValid: clockValid,
    decoderHardware: decoderHardware,
    queue: decoder ? decoder.decodeQueueSize : 0,
  };
}

function postStats() {
  self.postMessage({
    type: 'stats',
    framesIn, keyframesIn, framesDecoded, framesRendered,
    decoderState: decoder ? decoder.state : 'none',
    queue: decoder ? decoder.decodeQueueSize : 0,
    waitingForKey, configured, configError,
  });
}

function makeDecoder() {
  return new VideoDecoder({
    output: (frame) => {
      framesDecoded++;
      try {
        if (canvas && !sizedCanvas && frame.displayWidth) {
          canvas.width = frame.displayWidth;
          canvas.height = frame.displayHeight;
          sizedCanvas = true;
        }
        if (ctx && frame.timestamp >= renderFromTs) {
          ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
          framesRendered++;
          // The canvas path paints synchronously here, so this instant is the paint.
          const paintMs = nowAbsMs();
          const rec = pending.get(frame.timestamp);
          if (rec) {
            pending.delete(frame.timestamp);
            wArrivalToPaint.push(paintMs - rec.arrival);
            if (rec.submit) wDecode.push(paintMs - rec.submit);
          }
          if (clockValid) {
            const hostCaptureAbsMs = hostCaptureAbsMsOf(frame.timestamp);
            if (hostCaptureAbsMs !== null) wHostToPaint.push(paintMs - hostCaptureAbsMs);
          }
          reportAvSync(frame.timestamp);
          if (!renderedOnce) {
            renderedOnce = true;
            self.postMessage({ type: 'rendered' });
          }
        }
      } finally {
        frame.close();
      }
    },
    error: (err) => {
      decodeErrors++;
      self.postMessage({ type: 'decodeerror', message: String(err) });
      resetAndRequestKey();
    },
  });
}

async function ensureConfigured() {
  if (configured && decoder && decoder.state !== 'closed') return true;
  const candidates = preferredCodec
    ? [preferredCodec, ...CODEC_CANDIDATES.filter((c) => c !== preferredCodec)]
    : CODEC_CANDIDATES.slice();
  let codec = candidates[0];
  if (typeof VideoDecoder.isConfigSupported === 'function') {
    let anySupported = false;
    for (const c of candidates) {
      try {
        const s = await VideoDecoder.isConfigSupported({ codec: c });
        if (s && s.supported) {
          codec = c;
          anySupported = true;
          if (s.config && s.config.hardwareAcceleration) {
            decoderHardware = s.config.hardwareAcceleration !== 'prefer-software';
          }
          break;
        }
      } catch (_) {}
    }
    if (!anySupported) {
      configError = 'no supported H.264 decoder configuration';
      self.postMessage({ type: 'configerror', message: configError, codec });
      throw new Error(configError);
    }
  }
  decoder = makeDecoder();
  try {
    decoder.configure({ codec, optimizeForLatency: true });
  } catch (err) {
    configError = String(err);
    self.postMessage({ type: 'configerror', message: configError, codec });
    throw err;
  }
  configured = true;
  waitingForKey = true;
  return true;
}

function resetAndRequestKey() {
  configured = false;
  waitingForKey = true;
  try {
    if (decoder && decoder.state !== 'closed') decoder.close();
  } catch (_) {}
  decoder = null;
  requestKey(true);
}

function requestKey(force) {
  const now = (typeof performance !== 'undefined' ? performance.now() : Date.now());
  if (!force && now - lastKeyRequestAt < KEY_REQUEST_MIN_INTERVAL_MS) return;
  lastKeyRequestAt = now;
  keyRequests++;
  if (transformer && typeof transformer.sendKeyFrameRequest === 'function') {
    transformer.sendKeyFrameRequest().catch(() => {});
  }
}

let firstReadDone = false;

self.onrtctransform = (event) => {
  transformer = event.transformer;
  self.postMessage({
    type: 'transformstart',
    hasReadable: !!(transformer && transformer.readable),
    hasSendKey: !!(transformer && typeof transformer.sendKeyFrameRequest === 'function'),
  });
  startPump(transformer.readable);
};

function startPump(readable) {
  const reader = readable.getReader();

  ensureConfigured().then(() => requestKey(true)).catch(() => {});

  (async function pump() {
    for (;;) {
      let result;
      try {
        result = await reader.read();
      } catch (_) {
        break;
      }
      const { value: encodedFrame, done } = result;
      if (done) break;

      if (!firstReadDone) {
        firstReadDone = true;
        self.postMessage({ type: 'firstframe', frameType: encodedFrame.type });
      }

      framesIn++;
      const arrivalMs = nowAbsMs();
      const type = encodedFrame.type === 'key' ? 'key' : 'delta';
      if (type === 'key') keyframesIn++;

      if (waitingForKey && type !== 'key') {
        requestKey(false);
        continue;
      }

      if (renderedOnce && !waitingForKey && decoder && type !== 'key') {
        const backlogThreshold = Math.max(3, Math.round(estimatedFps * 0.05));
        if (decoder.decodeQueueSize > backlogThreshold) {
          backlogCount++;
          framesDroppedBacklog++;
          if (backlogCount >= 2) {
            self.postMessage({ type: 'backlog', size: decoder.decodeQueueSize });
            waitingForKey = true;
            backlogCount = 0;
            requestKey(false);
          }
          continue;
        } else {
          backlogCount = 0;
        }
      }

      try {
        if (!configured || !decoder || decoder.state === 'closed') await ensureConfigured();
        if (waitingForKey && type === 'key') {
          waitingForKey = false;
          renderFromTs = encodedFrame.timestamp;
        }
        notePending(encodedFrame.timestamp, arrivalMs);
        const rec = pending.get(encodedFrame.timestamp);
        if (rec) rec.submit = nowAbsMs();
        decoder.decode(
          new EncodedVideoChunk({
            type,
            timestamp: encodedFrame.timestamp,
            data: encodedFrame.data,
          }),
        );
      } catch (err) {
        decodeErrors++;
        self.postMessage({ type: 'decodeerror', message: String(err) });
        resetAndRequestKey();
      }
    }
  })();
}
