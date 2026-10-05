// The script injected into the client page before any of its own scripts run. Observation only:
// it wraps APIs to record what the page does, and never creates or dispatches an event of any
// kind (no pointer, keyboard, wheel, touch, focus or synthetic DOM events), so nothing it does
// can reach the host's input injector. It must work on every page generation (the old page has
// no SEVideo and keeps its state in top-level lexical bindings).

export function observerSource(opts = {}) {
  const o = JSON.stringify(opts);
  return `(() => {
  const OPTS = ${o};
  const S = window.__se = { gum: [], permQueries: [], pcs: [], whep: [], errors: [], analysers: [], onsets: [], audioBuckets: [], notes: [] };
  const now = () => performance.timeOrigin + performance.now();
  try {
    // self.__seRefresh is set by a later new-document script when the client spec changes (the
    // hi-res scenario reuses this browser), so the page reports the current spec's rate.
    if (OPTS.refreshRate) Object.defineProperty(Screen.prototype, 'refreshRate', { get: () => self.__seRefresh || OPTS.refreshRate, configurable: true });
    if (OPTS.disable && OPTS.disable.indexOf('webcodecs') >= 0) { delete window.VideoDecoder; delete window.EncodedVideoChunk; }
    if (OPTS.disable && OPTS.disable.indexOf('script_transform') >= 0) { delete window.RTCRtpScriptTransform; }
    if (OPTS.disable && OPTS.disable.indexOf('encoded_streams') >= 0 && window.RTCRtpReceiver) { delete RTCRtpReceiver.prototype.createEncodedStreams; }
    // Audio tiers. 'sab': no SharedArrayBuffer although the page is isolated (the private ring
    // over a MessagePort). 'audio_decoder': no WebCodecs audio (the standard Opus track from the
    // start). 'audio_worker': the decode worker's script fails to load after the page has
    // already claimed the fast path (the mid-session step-down).
    if (OPTS.disable && OPTS.disable.indexOf('sab') >= 0) { delete window.SharedArrayBuffer; }
    if (OPTS.disable && OPTS.disable.indexOf('audio_decoder') >= 0) { delete window.AudioDecoder; delete window.EncodedAudioChunk; }
    if (OPTS.disable && OPTS.disable.indexOf('audio_worker') >= 0) {
      const Wk = window.Worker;
      const Wrapped = function (u, o) { const s = String(u); return new Wk(/audio-worker\\.js/.test(s) ? s.replace('audio-worker.js', 'se-handler-missing-worker.js') : u, o); };
      Wrapped.prototype = Wk.prototype; window.Worker = Wrapped;
    }
  } catch (e) { S.notes.push('disable failed ' + e); }
  // Microphone: was a capture permission requested, and by what.
  try {
    const md = navigator.mediaDevices;
    if (md && md.getUserMedia) { const g = md.getUserMedia.bind(md); md.getUserMedia = function (c) { S.gum.push({ t: now(), constraints: JSON.stringify(c), stack: (new Error().stack || '').split('\\n').slice(2, 4).join(' | ') }); return g(c); }; }
    if (navigator.permissions && navigator.permissions.query) { const q = navigator.permissions.query.bind(navigator.permissions); navigator.permissions.query = function (d) { S.permQueries.push({ t: now(), name: d && d.name }); return q(d); }; }
  } catch (e) { S.notes.push('mic wrap failed ' + e); }
  // Peer connections, for getStats.
  try {
    const P = window.RTCPeerConnection;
    const Wrapped = function (c) { const pc = new P(c); S.pcs.push(pc); return pc; };
    Wrapped.prototype = P.prototype;
    for (const k of Object.getOwnPropertyNames(P)) { try { if (!(k in Wrapped)) Wrapped[k] = P[k]; } catch (_) {} }
    window.RTCPeerConnection = Wrapped;
  } catch (e) { S.notes.push('pc wrap failed ' + e); }
  // Join timing: when the /whep answer arrived, from the page's point of view.
  try {
    const F = window.fetch;
    window.fetch = function (u, init) {
      const url = String(u && u.url || u); const m = (init && init.method) || 'GET';
      const isWhep = /\\/whep/.test(url); const t0 = now();
      const p = F.apply(this, arguments);
      if (isWhep) p.then((r) => { S.whep.push({ method: m, sent: t0, answered: now(), status: r.status }); }, (e) => { S.whep.push({ method: m, sent: t0, failed: now(), error: String(e) }); });
      return p;
    };
  } catch (e) { S.notes.push('fetch wrap failed ' + e); }
  window.addEventListener('error', (e) => { S.errors.push(String(e.message)); });
  // Audio: an analyser spliced in before every connection to the destination.
  try {
    const C = AudioNode.prototype.connect;
    AudioNode.prototype.connect = function (d) {
      try {
        if (d instanceof AudioDestinationNode && !this.__seTap) {
          const an = this.context.createAnalyser(); an.fftSize = 2048; an.__seTap = true; an.smoothingTimeConstant = 0;
          C.call(this, an); S.analysers.push({ an, ctx: this.context, last: 0, quietSince: 0, armed: false });
        }
      } catch (_) {}
      return C.apply(this, arguments);
    };
  } catch (e) { S.notes.push('audio tap failed ' + e); }
  // Media elements: every srcObject assignment, each element's load/play/pause history, and an
  // analyser on any stream carrying a track a peer connection received (the standard Opus track,
  // which never passes through the page's own audio graph). The tap lives in its own context.
  S.mediaEls = [];
  try {
    const desc = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, 'srcObject');
    const remoteIds = () => { const ids = new Set(); for (const pc of S.pcs) { try { for (const r of pc.getReceivers()) if (r.track) ids.add(r.track.id); } catch (_) {} } return ids; };
    Object.defineProperty(HTMLMediaElement.prototype, 'srcObject', {
      configurable: true, enumerable: desc.enumerable, get: desc.get,
      set: function (v) {
        let rec = S.mediaEls.find((r) => r.el === this);
        if (!rec) {
          rec = { el: this, tag: this.tagName, assigns: [], events: {} };
          S.mediaEls.push(rec);
          for (const ev of ['loadstart', 'playing', 'pause', 'emptied', 'error', 'stalled', 'waiting']) this.addEventListener(ev, () => { rec.events[ev] = (rec.events[ev] || 0) + 1; });
        }
        const tracks = v && v.getAudioTracks ? v.getAudioTracks() : [];
        const remote = remoteIds(); const isRemote = tracks.some((t) => remote.has(t.id));
        rec.assigns.push({ t: now(), stream: v ? v.id : null, audioTracks: tracks.length, remote: isRemote });
        desc.set.call(this, v);
        if (isRemote && this.tagName === 'AUDIO' && !S.trackTaps) S.trackTaps = [];
        if (isRemote && this.tagName === 'AUDIO' && !S.trackTaps.some((x) => x.stream === v.id)) {
          try {
            const ctx = new AudioContext(); const src = ctx.createMediaStreamSource(v); const an = ctx.createAnalyser(); an.fftSize = 2048; an.smoothingTimeConstant = 0; an.__seTap = true;
            src.connect(an); S.analysers.push({ an, ctx, last: 0, quietSince: 0, armed: false, kind: 'element-track' }); S.trackTaps.push({ stream: v.id });
            if (ctx.state === 'suspended') ctx.resume().catch(() => {});
          } catch (e) { S.notes.push('track tap failed ' + e); }
        }
      },
    });
  } catch (e) { S.notes.push('srcObject wrap failed ' + e); }
  S.mediaSummary = function () {
    return S.mediaEls.map((r) => ({ tag: r.tag, assigns: r.assigns.length, lastAssign: r.assigns[r.assigns.length - 1], events: r.events, paused: r.el.paused, currentTime: r.el.currentTime, sinkId: typeof r.el.sinkId === 'string' ? r.el.sinkId : null, muted: r.el.muted, hasSrc: !!desc0(r.el) }));
    function desc0(el) { try { return el.srcObject; } catch (_) { return null; } }
  };
  // Onset detection, started by the harness for the audio window.
  S.startOnsets = function (threshold, quietMs) {
    threshold = threshold || 0.02; quietMs = quietMs || 30;
    if (S.onsetTimer) return;
    let bucket = { t: now(), sumsq: 0, n: 0, peak: 0 };
    S.onsetTimer = setInterval(() => {
      for (const a of S.analysers) {
        const ctx = a.ctx; if (ctx.state !== 'running') continue;
        const sr = ctx.sampleRate, N = a.an.fftSize, buf = new Float32Array(N);
        a.an.getFloatTimeDomainData(buf);
        const ct = ctx.currentTime, wall = now(); let ots = null; try { ots = ctx.getOutputTimestamp(); } catch (_) {}
        const startCt = ct - N / sr;
        for (let i = 0; i < N; i++) {
          const tCt = startCt + i / sr; if (tCt <= a.last) continue;
          const v = Math.abs(buf[i]);
          bucket.sumsq += buf[i] * buf[i]; bucket.n++; if (v > bucket.peak) bucket.peak = v;
          if (v < threshold / 2) { if (!a.quietSince) a.quietSince = tCt; if ((tCt - a.quietSince) * 1000 >= quietMs) a.armed = true; }
          else a.quietSince = 0;
          if (a.armed && v >= threshold) {
            a.armed = false; a.quietSince = 0;
            let peak = 0; for (let k = i; k < Math.min(N, i + Math.round(sr * 0.006)); k++) peak = Math.max(peak, Math.abs(buf[k]));
            const render = wall - (ct - tCt) * 1000;
            const output = ots && ots.performanceTime ? performance.timeOrigin + ots.performanceTime + (tCt - ots.contextTime) * 1000 : null;
            S.onsets.push({ render, output, peak: +peak.toFixed(4), outputLatencyMs: ctx.outputLatency ? ctx.outputLatency * 1000 : null, baseLatencyMs: ctx.baseLatency ? ctx.baseLatency * 1000 : null });
          }
        }
        a.last = ct;
      }
      if (now() - bucket.t >= 250) { S.audioBuckets.push({ t: bucket.t, rms: bucket.n ? +Math.sqrt(bucket.sumsq / bucket.n).toFixed(5) : null, peak: +bucket.peak.toFixed(4) }); bucket = { t: now(), sumsq: 0, n: 0, peak: 0 }; }
    }, 5);
  };
  S.stopOnsets = function () { clearInterval(S.onsetTimer); S.onsetTimer = null; };
})();`;
}

/**
 * Installed in every worker: an onset detector on decoded audio. It wraps `AudioData.copyTo`
 * (every page generation's decode worker copies plane 0 of each decoded packet out with it) and
 * notes, per onset, when the worker had it decoded and which capture time the packet carried.
 * That splits the audio delay at the decoder: host → decoded, and decoded → rendered. Observation
 * only; the copy itself is untouched.
 */
export const AUDIO_DECODE_TAP_SOURCE = `(() => {
  if (typeof AudioData !== 'function' || self.__seAudTap) return;
  const T = self.__seAudTap = { onsets: [], armed: false, quietSince: null, packets: 0 };
  const C = AudioData.prototype.copyTo;
  AudioData.prototype.copyTo = function (dest, opts) {
    const r = C.apply(this, arguments);
    try {
      if (opts && opts.planeIndex === 0 && dest && dest.length) {
        T.packets++;
        const now = performance.timeOrigin + performance.now(); const n = Math.min(dest.length, this.numberOfFrames || dest.length); const sr = this.sampleRate || 48000;
        for (let i = 0; i < n; i++) {
          const v = Math.abs(dest[i]); const tUs = this.timestamp + i * 1e6 / sr;
          if (v < 0.01) { if (T.quietSince === null) T.quietSince = tUs; if (tUs - T.quietSince >= 30000) T.armed = true; }
          else T.quietSince = null;
          if (T.armed && v >= 0.02) { T.armed = false; T.quietSince = null; T.onsets.push({ decoded: now, captureUs: tUs, peak: v }); if (T.onsets.length > 500) T.onsets.shift(); }
        }
      }
    } catch (_) {}
    return r;
  };
})();`;

/** Evaluated in the page: where the live picture is and how big its frames are. */
export const GEOMETRY_EXPR = `(() => {
  const vis = (el) => { if (!el) return false; const s = getComputedStyle(el); const r = el.getBoundingClientRect(); return s.display !== 'none' && s.visibility !== 'hidden' && r.width > 10 && r.height > 10; };
  const cv = document.getElementById('canvas'), vd = document.getElementById('video');
  let el = null, kind = null, fw = 0, fh = 0;
  if (vis(vd) && vd.videoWidth) { el = vd; kind = 'video'; fw = vd.videoWidth; fh = vd.videoHeight; }
  else if (vis(cv)) { el = cv; kind = 'canvas'; fw = cv.width; fh = cv.height; }
  if (!el) return { live: false, vw: innerWidth, vh: innerHeight, dpr: devicePixelRatio };
  const r = el.getBoundingClientRect(); const fit = getComputedStyle(el).objectFit || 'fill';
  let cx = r.left, cy = r.top, cw = r.width, ch = r.height;
  if (fw && fh && (fit === 'contain' || kind === 'video')) { const s = Math.min(r.width / fw, r.height / fh); cw = fw * s; ch = fh * s; cx = r.left + (r.width - cw) / 2; cy = r.top + (r.height - ch) / 2; }
  return { live: true, kind, fit, fw, fh, x: cx, y: cy, w: cw, h: ch, rx: r.left, ry: r.top, rw: r.width, rh: r.height, vw: innerWidth, vh: innerHeight, dpr: devicePixelRatio };
})()`;

/** Evaluated in the page: everything the build reports about itself, whatever it has. */
export const SECONDARY_EXPR = `(async () => {
  const o = {};
  try { o.seVideo = window.SEVideo && SEVideo.getStats ? SEVideo.getStats() : null; } catch (e) { o.seVideo = 'ERR ' + e.message; }
  try { o.seAudio = window.SEAudio && SEAudio.getSyncInfo ? SEAudio.getSyncInfo() : null; } catch (e) { o.seAudio = 'ERR ' + e.message; }
  try { o.audioActivePath = window.SEAudio && SEAudio.activePath ? SEAudio.activePath() : null; } catch (_) {}
  try { o.usingFallback = typeof usingFallback !== 'undefined' ? usingFallback : undefined; } catch (_) {}
  try { o.isLive = typeof isLive !== 'undefined' ? isLive : undefined; } catch (_) {}
  const pcs = (window.__se && __se.pcs) || [];
  const pc = pcs.filter((p) => p.connectionState !== 'closed').pop() || pcs[pcs.length - 1];
  o.peerConnections = pcs.length; o.openPeerConnections = pcs.filter((p) => p.connectionState !== 'closed').length;
  if (pc) {
    const r = await pc.getStats(); const codecs = {}; const w = { dataChannels: [] };
    r.forEach((s) => { if (s.type === 'codec') codecs[s.id] = s.mimeType + ' ' + (s.sdpFmtpLine || ''); });
    r.forEach((s) => {
      if (s.type === 'inbound-rtp' && s.kind === 'video') w.video = { codec: codecs[s.codecId], decoder: s.decoderImplementation, powerEfficientDecoder: s.powerEfficientDecoder, packets: s.packetsReceived, lost: s.packetsLost, framesReceived: s.framesReceived, framesDecoded: s.framesDecoded, framesDropped: s.framesDropped, keyFrames: s.keyFramesDecoded, pli: s.pliCount, fir: s.firCount, nack: s.nackCount, jitterMs: s.jitter != null ? s.jitter * 1000 : null, frameWidth: s.frameWidth, frameHeight: s.frameHeight, fps: s.framesPerSecond, jitterBufferDelayMs: s.jitterBufferEmittedCount ? 1000 * s.jitterBufferDelay / s.jitterBufferEmittedCount : null, bytes: s.bytesReceived };
      if (s.type === 'inbound-rtp' && s.kind === 'audio') w.audioTrack = { codec: codecs[s.codecId], packets: s.packetsReceived, lost: s.packetsLost, concealed: s.concealedSamples, jitterMs: s.jitter != null ? s.jitter * 1000 : null, jitterBufferDelayMs: s.jitterBufferEmittedCount ? 1000 * s.jitterBufferDelay / s.jitterBufferEmittedCount : null, jitterBufferTargetMs: s.jitterBufferEmittedCount && s.jitterBufferTargetDelay != null ? 1000 * s.jitterBufferTargetDelay / s.jitterBufferEmittedCount : null, samplesReceived: s.totalSamplesReceived, audioLevel: s.audioLevel };
      if (s.type === 'data-channel') w.dataChannels.push({ label: s.label, state: s.state, messagesReceived: s.messagesReceived, bytesReceived: s.bytesReceived });
      if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') w.candidatePair = { rttMs: s.currentRoundTripTime != null ? s.currentRoundTripTime * 1000 : null, availableIncomingBitrate: s.availableIncomingBitrate, bytesReceived: s.bytesReceived };
    });
    o.webrtc = w; o.connectionState = pc.connectionState;
  }
  return o;
})()`;
