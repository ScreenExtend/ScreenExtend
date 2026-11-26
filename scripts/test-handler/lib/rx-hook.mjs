// Per encoded video frame, as the page's transform worker reads it: RTP timestamp, the read
// instant, getMetadata().receiveTime and the host capture time the worker derives (rows of 6).
// Wraps ReadableStreamDefaultReader.read because both transform tiers end in reader.read().
// Times are performance.timeOrigin + now() on the worker's clock.

export const RX_HOOK_SOURCE = `(() => {
  if (self.__seRx) return;
  const N = 1 << 16, W = 6;
  const g = self.__seRx = { on: false, k: 0, buf: new Float64Array(N * W), fields: null, sample: null, err: null, dropped: 0 };
  const RD = self.ReadableStreamDefaultReader && ReadableStreamDefaultReader.prototype;
  if (!RD || !RD.read) { g.err = 'no ReadableStreamDefaultReader'; return; }
  const r0 = RD.read;
  RD.read = function () {
    return r0.apply(this, arguments).then(function (res) {
      if (!g.on) return res;
      const t = performance.timeOrigin + performance.now();
      try {
        const f = res && res.value;
        if (f && typeof f.getMetadata === 'function' && typeof f.timestamp === 'number' && typeof f.type === 'string') {
          const md = f.getMetadata() || {};
          if (!g.fields) { g.fields = Object.keys(md); g.sample = {}; for (const k of g.fields) { const v = md[k]; g.sample[k] = (typeof v === 'object' && v !== null) ? '[' + (v.constructor && v.constructor.name) + ']' : v; } }
          if (g.k >= N) { g.dropped++; return res; }
          let cap = -1;
          try { if (typeof hostCaptureAbsMsOf === 'function') { const a = hostCaptureAbsMsOf(f.timestamp); if (a !== null && isFinite(a)) cap = a; } } catch (e) { g.err = String(e); }
          const o = g.k * W; g.k++;
          g.buf[o] = f.timestamp >>> 0; g.buf[o + 1] = t;
          g.buf[o + 2] = typeof md.receiveTime === 'number' ? md.receiveTime + performance.timeOrigin : -1;
          g.buf[o + 3] = cap; g.buf[o + 4] = f.type === 'key' ? 1 : 0; g.buf[o + 5] = f.data ? f.data.byteLength : 0;
        }
      } catch (e) { g.err = String(e); }
      return res;
    });
  };
})();`;

export const RX_ARM = `self.__seRx ? (__seRx.k = 0, __seRx.dropped = 0, __seRx.on = true, true) : false`;
export const RX_READ = `self.__seRx ? (() => { const g = __seRx; g.on = false; return { n: g.k, rows: Array.from(g.buf.subarray(0, g.k * 6)), fields: g.fields, sample: g.sample, err: g.err, dropped: g.dropped, origin: performance.timeOrigin }; })() : null`;

/** Rows from RX_READ -> objects. `shift` moves worker-clock times onto the reference clock. */
export function rxRows(v, shift = 0) {
  const out = [];
  if (!v || !v.rows) return out;
  for (let i = 0; i + 5 < v.rows.length; i += 6) {
    const r = v.rows;
    out.push({ rtp: r[i], read: r[i + 1] + shift, recv: r[i + 2] > 0 ? r[i + 2] + shift : null, cap: r[i + 3] > 0 ? r[i + 3] + shift : null, key: r[i + 4] === 1, bytes: r[i + 5] });
  }
  return out;
}

const r2 = (x) => (x == null || !isFinite(x) ? null : Math.round(x * 100) / 100);
function pct(a) {
  const s = a.filter((x) => isFinite(x)).sort((x, y) => x - y);
  const q = (p) => (s.length ? r2(s[Math.min(s.length - 1, Math.floor(p * s.length))]) : null);
  const mean = s.length ? s.reduce((x, y) => x + y, 0) / s.length : null;
  return { n: s.length, min: s.length ? r2(s[0]) : null, p10: q(0.1), p50: q(0.5), p90: q(0.9), p99: q(0.99), max: s.length ? r2(s[s.length - 1]) : null, mean: r2(mean) };
}
function hist(a, lo, hi, bins) {
  const h = new Array(bins).fill(0); const w = (hi - lo) / bins; let under = 0, over = 0;
  for (const x of a) { if (x < lo) under++; else if (x >= hi) over++; else h[Math.min(bins - 1, Math.floor((x - lo) / w))]++; }
  return { lo, hi, binMs: r2(w), counts: h, under, over };
}
function phaseLock1(ts, P) {
  let c = 0, s = 0;
  for (const t of ts) { const a = (2 * Math.PI * (((t % P) + P) % P)) / P; c += Math.cos(a); s += Math.sin(a); }
  return ts.length ? Math.hypot(c, s) / ts.length : 0;
}
// Mean resultant length of instants folded onto period P: ~1 = all at one phase, ~0 = none.
// For n uniform points it is ~1/sqrt(n) by chance; `rayleighZ` = n R^2 (> 10 is p < 5e-5).
function phaseLock(ts, P) {
  if (!ts.length) return null;
  let c = 0, s = 0;
  for (const t of ts) { const a = (2 * Math.PI * (((t % P) + P) % P)) / P; c += Math.cos(a); s += Math.sin(a); }
  const R = Math.hypot(c, s) / ts.length;
  const mean = ((Math.atan2(s, c) / (2 * Math.PI)) * P + P) % P;
  // share of instants within +-1 ms of the mean phase (a band 2 ms wide; 2/P by chance)
  let near = 0; for (const t of ts) { let d = Math.abs((((t - mean) % P) + P) % P); d = Math.min(d, P - d); if (d <= 1) near++; }
  return { R: +R.toFixed(3), rayleighZ: +(ts.length * R * R).toFixed(1), meanPhaseMs: r2(mean), within1msOfMean: +(near / ts.length).toFixed(3), chanceWithin1ms: +(Math.min(1, 2 / P)).toFixed(3), hist16: hist(ts.map((t) => (((t % P) + P) % P)), 0, P, 16).counts };
}

/**
 * The analysis for one window. `rows` from rxRows (reference clock), `relay` optionally
 * [{ rtp, lastAt }] the relay's own arrival of each frame's last (marker) packet on the same
 * reference clock. `sourceHz` is what the host was asked to send.
 */
export function analyzeRx(rows, { sourceHz = null, relay = null, displayHz = [60, 360] } = {}) {
  const vid = rows.filter((r) => r.bytes > 0);
  // duplicate RTP timestamps (a frame read twice) cannot happen on one reader; keep the first
  const seen = new Set(); const fr = [];
  for (const r of vid) { if (seen.has(r.rtp)) continue; seen.add(r.rtp); fr.push(r); }
  fr.sort((a, b) => a.read - b.read);
  const out = { frames: fr.length, keyframes: fr.filter((r) => r.key).length };
  if (fr.length < 2) return { ...out, error: 'fewer than two frames read' };
  const span = (fr[fr.length - 1].read - fr[0].read) / 1000;
  out.windowS = r2(span); out.readFps = r2((fr.length - 1) / span);
  // source cadence, from the host's own RTP clock (90 kHz)
  const rtpIv = []; for (let i = 1; i < fr.length; i++) { const d = ((fr[i].rtp - fr[i - 1].rtp) >>> 0) / 90; if (d > 0 && d < 1000) rtpIv.push(d); }
  out.sourceIntervalMs = pct(rtpIv);
  out.sourceFps = rtpIv.length ? r2(1000 / (rtpIv.reduce((a, b) => a + b, 0) / rtpIv.length)) : null;
  const reads = fr.map((r) => r.read);
  const iv = []; for (let i = 1; i < reads.length; i++) iv.push(reads[i] - reads[i - 1]);
  out.readIntervalMs = pct(iv);
  out.readIntervalHist = hist(iv, 0, 50, 50);
  const M = 1000 / 64;
  // frames released together: the second read of a pair within 1 ms of the first
  out.pairs = { within1ms: iv.filter((x) => x < 1).length, share: +(iv.filter((x) => x < 1).length / iv.length).toFixed(3), note: 'reads less than 1 ms after the previous read (two frames handed to the worker by one release)' };
  // inter-read intervals that are a multiple of the 64 Hz period (+-1.5 ms) vs of the source period
  const nearMult = (x, P) => { const k = Math.round(x / P); return k >= 1 && Math.abs(x - k * P) <= 1.5; };
  out.intervalsNearMultipleOf15_625 = +(iv.filter((x) => nearMult(x, M)).length / iv.length).toFixed(3);
  if (sourceHz) out.intervalsNearMultipleOfSourcePeriod = +(iv.filter((x) => nearMult(x, 1000 / sourceHz)).length / iv.length).toFixed(3);
  const periods = { '15.625 (64 Hz metronome / Windows default timer tick)': M };
  for (const hz of displayHz) periods[`${(1000 / hz).toFixed(3)} (${hz} Hz display)`] = 1000 / hz;
  if (sourceHz) periods[`${(1000 / sourceHz).toFixed(3)} (${sourceHz} Hz source)`] = 1000 / sourceHz;
  out.phaseLock = { read: {}, receive: {}, relayLastPacket: {} };
  const recvs = fr.filter((r) => r.recv != null).map((r) => r.recv);
  for (const [name, P] of Object.entries(periods)) {
    out.phaseLock.read[name] = phaseLock(reads, P);
    out.phaseLock.receive[name] = recvs.length ? phaseLock(recvs, P) : null;
  }
  // Which period are the reads locked to? Scan 12-20 ms in 2.5 us steps for the largest R. A
  // 64 Hz release clock peaks at 15.625; the source alone peaks at its own period.
  const scan = (ts) => { let best = null; for (let P = 12; P <= 20; P += 0.0025) { const r = phaseLock1(ts, P); if (!best || r > best.R) best = { P, R: r }; } return best ? { periodMs: +best.P.toFixed(4), R: +best.R.toFixed(3) } : null; };
  out.bestPeriod = { read: scan(reads) };
  // release groups: reads within 1 ms of the previous one are one release
  const groups = [];
  for (const t of reads) { const g = groups[groups.length - 1]; if (g && t - g.last < 1) { g.n++; g.last = t; } else groups.push({ first: t, last: t, n: 1 }); }
  const gsz = {}; for (const g of groups) gsz[g.n] = (gsz[g.n] || 0) + 1;
  out.releaseGroups = { groups: groups.length, perSecond: r2(groups.length / span), sizes: gsz, startLock15_625: phaseLock(groups.map((g) => g.first), M), bestPeriod: scan(groups.map((g) => g.first)) };
  // per-second phase lock at 15.625 ms: a clock that is real but late under load keeps a high
  // per-second R with a wandering mean; no clock gives R ~ 1/sqrt(n) everywhere
  const perS = []; const t0 = reads[0];
  for (let s = 0; t0 + s * 1000 <= reads[reads.length - 1]; s++) { const w = reads.filter((t) => t >= t0 + s * 1000 && t < t0 + (s + 1) * 1000); if (w.length >= 8) { const p = phaseLock(w, M); perS.push({ R: p.R, ph: p.meanPhaseMs }); } }
  out.perSecondLock15_625 = { seconds: perS.length, R: pct(perS.map((x) => x.R)), meanPhaseMs: perS.map((x) => x.ph) };
  if (recvs.length) out.bestPeriod.receive = scan(recvs);
  // the hold, as Chrome reports it: read - receiveTime (last packet in -> transform reads it)
  const withRecv = fr.filter((r) => r.recv != null);
  out.receiveTimeAvailable = withRecv.length;
  if (withRecv.length) {
    const hold = withRecv.map((r) => r.read - r.recv);
    out.receive_to_read_ms = pct(hold);
    out.receiveToReadHist = hist(hold, 0, 32, 32);
    // if a 64 Hz clock releases the queue, read = next tick after receive (+ a post): estimate
    // the tick phase from the reads, predict each frame's release, report the residual
    const pl = phaseLock(reads, M);
    if (pl) {
      const ph = pl.meanPhaseMs;
      const resid = []; const predHold = [];
      for (const r of withRecv) {
        let tick = Math.ceil((r.recv - ph) / M) * M + ph; if (tick < r.recv) tick += M;
        predHold.push(tick - r.recv); resid.push(r.read - tick);
      }
      out.tickModel = { tickPhaseMs: ph, predictedHold_ms: pct(predHold), readMinusPredictedTick_ms: pct(resid), note: 'tick phase from the circular mean of the reads mod 15.625 ms; predictedHold = next tick after receiveTime - receiveTime; a real metronome makes readMinusPredictedTick small and tight (the cross-thread post), and predictedHold uniform on [0, 15.6)' };
    }
  }
  if (relay && relay.length) {
    const rm = new Map(relay.map((x) => [x.rtp >>> 0, x.lastAt]));
    const d = []; const dr = [];
    for (const r of fr) { const a = rm.get(r.rtp >>> 0); if (a == null) continue; d.push(r.read - a); if (r.recv != null) dr.push(r.recv - a); }
    out.relayMatched = d.length;
    out.relay_to_read_ms = pct(d); out.relayToReadHist = hist(d, 0, 32, 32);
    out.relay_to_receive_ms = pct(dr);
    const lp = relay.map((x) => x.lastAt);
    for (const [name, P] of Object.entries(periods)) out.phaseLock.relayLastPacket[name] = phaseLock(lp, P);
  }
  const caps = fr.filter((r) => r.cap != null);
  if (caps.length) {
    out.capture_to_read_ms = pct(caps.map((r) => r.read - r.cap));
    const cr = caps.filter((r) => r.recv != null);
    if (cr.length) out.capture_to_receive_ms = pct(cr.map((r) => r.recv - r.cap));
  }
  return out;
}
