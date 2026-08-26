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

// A/V sync (PRD §6.5): the host stamps each video frame's RTP timestamp from its monotonic epoch
// at 90 kHz (see webrtc_session.rs). We invert that per drawn frame to recover the frame's
// host-capture time, then report how far the client's *display* lags host capture. The audio path
// consumes this to align playout to the picture. All timing uses timeOrigin+now() so the number is
// comparable across the worker and the main/audio threads.
const RTP_HZ = 90000;
let rtpUnwrapLast = null;
let rtpWraps = 0;
let videoDelayEmaMs = null;
let lastAvsyncPostMs = 0;

const CODEC_CANDIDATES = [
  'avc1.640034', 'avc1.640028', 'avc1.64001F',
  'avc1.4D4034', 'avc1.4D401F',
  'avc1.42E034', 'avc1.42E01F', 'avc1.42001F',
];

const KEY_REQUEST_MIN_INTERVAL_MS = 250;
const BACKLOG_DROP_THRESHOLD = 3;

let preferredCodec = null;

self.onmessage = (e) => {
  if (e.data && e.data.type === 'canvas') {
    canvas = e.data.canvas;
    ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
  } else if (e.data && e.data.type === 'codechint') {
    if (e.data.profileLevelId && /^[0-9a-fA-F]{6}$/.test(e.data.profileLevelId)) {
      preferredCodec = 'avc1.' + e.data.profileLevelId.toUpperCase();
    }
  } else if (e.data && e.data.type === 'stats') {
    postStats();
  } else if (e.data && e.data.type === 'rendercount') {
    self.postMessage({ type: 'rendercount', n: framesRendered });
  } else if (e.data && e.data.type === 'encodedstream') {
    self.postMessage({ type: 'transformstart', hasReadable: !!e.data.readable, hasSendKey: false });
    startPump(e.data.readable);
  }
};

// Recover a drawn frame's host-capture time from its (host-clock, 90 kHz) RTP timestamp and post
// an EMA of the display lag to the main thread. `tsTicks` is the u32 RTP timestamp echoed through
// the decoder; we unwrap it to survive the ~13.25 h wrap so the lag stays stable mid-session.
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
  const delta = drawAbsMs - videoHostMs;
  if (videoDelayEmaMs === null) {
    videoDelayEmaMs = delta;
  } else {
    const a = delta < videoDelayEmaMs ? 0.30 : 0.05;
    videoDelayEmaMs = videoDelayEmaMs * (1 - a) + delta * a;
  }
  const nowMs = performance.now();
  if (nowMs - lastAvsyncPostMs > 200) {
    lastAvsyncPostMs = nowMs;
    self.postMessage({ type: 'avsync', videoDelayMs: videoDelayEmaMs });
  }
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
        if (s && s.supported) { codec = c; anySupported = true; break; }
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
    decoder.configure({ codec });
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
        requestKey(false);
        continue;
      }

      try {
        if (!configured || !decoder || decoder.state === 'closed') await ensureConfigured();
        if (waitingForKey && type === 'key') {
          waitingForKey = false;
          renderFromTs = encodedFrame.timestamp;
        }
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
}
