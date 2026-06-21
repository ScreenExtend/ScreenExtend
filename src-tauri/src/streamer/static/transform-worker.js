let canvas = null;
let ctx = null;
let decoder = null;
let transformer = null;
let configured = false;
let waitingForKey = true;
let sizedCanvas = false;
let renderedOnce = false;

let framesIn = 0;
let keyframesIn = 0;
let framesDecoded = 0;
let framesRendered = 0;
let lastKeyRequestAt = 0;
let configError = null;

const CODEC_CANDIDATES = [
  'avc1.640034', 'avc1.640028', 'avc1.64001F',
  'avc1.4D4034', 'avc1.4D401F',
  'avc1.42E034', 'avc1.42E01F', 'avc1.42001F',
];

const KEY_REQUEST_MIN_INTERVAL_MS = 250;
// 4 (was 8): drop to the freshest frame sooner, halving worst-case standing
// decoder latency. NOT 2-3 — natural decodeQueueSize on a weak iGPU at 1080p
// sits ~1-3 under load, so a lower threshold would fire nearly every frame and
// the drop→forced-IDR→refill cycle would livelock the already-saturated encoder.
const BACKLOG_DROP_THRESHOLD = 4;

self.onmessage = (e) => {
  if (e.data && e.data.type === 'canvas') {
    canvas = e.data.canvas;
    ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
  } else if (e.data && e.data.type === 'stats') {
    postStats();
  }
};

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
        if (ctx) {
          ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
          framesRendered++;
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
      self.postMessage({ type: 'decodeerror', message: String(err) });
      resetAndRequestKey();
    },
  });
}

async function ensureConfigured() {
  if (configured && decoder && decoder.state !== 'closed') return true;
  let codec = CODEC_CANDIDATES[0];
  if (typeof VideoDecoder.isConfigSupported === 'function') {
    for (const c of CODEC_CANDIDATES) {
      try {
        const s = await VideoDecoder.isConfigSupported({ codec: c, optimizeForLatency: true });
        if (s && s.supported) { codec = c; break; }
      } catch (_) {}
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
  const reader = transformer.readable.getReader();

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
      const type = encodedFrame.type === 'key' ? 'key' : 'delta';
      if (type === 'key') keyframesIn++;

      if (waitingForKey && type !== 'key') {
        requestKey(false);
        continue;
      }

      if (renderedOnce && !waitingForKey && decoder &&
          decoder.decodeQueueSize > BACKLOG_DROP_THRESHOLD && type !== 'key') {
        self.postMessage({ type: 'backlog', size: decoder.decodeQueueSize });
        waitingForKey = true;
        // Throttled (not forced): at the lower BACKLOG_DROP_THRESHOLD this can
        // trip more often, so honor the 250ms min interval to avoid hammering
        // the weak encoder with back-to-back IDRs (each a serialization spike).
        requestKey(false);
        continue;
      }

      try {
        await ensureConfigured();
        if (waitingForKey && type === 'key') waitingForKey = false;
        decoder.decode(
          new EncodedVideoChunk({
            type,
            timestamp: encodedFrame.timestamp,
            data: encodedFrame.data,
          }),
        );
      } catch (err) {
        self.postMessage({ type: 'decodeerror', message: String(err) });
        resetAndRequestKey();
      }
    }
  })();
};
