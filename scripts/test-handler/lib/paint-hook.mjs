// The readback that produces the primary metric. Installed in every worker the client page
// starts (before the worker's own script runs, through the debugger's auto-attach) and in the
// page itself. It wraps `drawImage` on 2D canvas contexts: when the page paints a decoded
// VideoFrame, the hook stamps the moment of the paint call and reads the clock stamp out of
// *that frame* — a clone, cropped to the stamp strip, read asynchronously so the paint itself
// is not delayed. For the <video> path the page script samples the element on
// requestVideoFrameCallback instead (expectedDisplayTime). Both page generations paint with
// `ctx.drawImage(frame, ...)` in their transform worker, so the same hook covers old and new.
//
// Off (cfg = null) it costs one property read per paint. It never creates or dispatches events.

export const HOOK_SOURCE = `(() => {
  if (self.__seStamp) return;
  const S = self.__seStamp = { cfg: null, out: [], pending: 0, prev: null, painted: 0, sampled: 0, skipped: 0, errors: 0, diag: 0 };
  const now = () => performance.timeOrigin + performance.now();
  const CELLS = 38, PX = 4, W = CELLS * PX;
  let strip = null, grid = null;
  function decode(d) {
    const l = [];
    for (let i = 0; i < CELLS; i++) { const o = (i * PX + (PX >> 1)) * 4; l.push(0.299 * d[o] + 0.587 * d[o + 1] + 0.114 * d[o + 2]); }
    const white = (l[0] + l[2] + l[37]) / 3, black = (l[1] + l[3] + l[36]) / 3;
    if (white - black < 60) return { e: 'guard' };
    const th = (white + black) / 2; const b = l.map((x) => (x > th ? 1 : 0));
    if (b[0] !== 1 || b[1] !== 0 || b[2] !== 1 || b[3] !== 0 || b[36] !== 0 || b[37] !== 1) return { e: 'guard' };
    let v = 0; for (let i = 0; i < 24; i++) v = v * 2 + b[4 + i];
    let c = 0; for (let i = 0; i < 8; i++) c = c * 2 + b[28 + i];
    const sum = (((v >> 16) & 255) + ((v >> 8) & 255) + (v & 255)) & 255;
    if ((sum ^ 0x5a) !== c) return { e: 'checksum', l: l.map(Math.round) };
    return { v, contrast: Math.round(white - black) };
  }
  // src: a VideoFrame or an HTMLVideoElement; fw: its natural width; t: paint time.
  S.sample = function (src, fw, fh, t) {
    const cfg = S.cfg; if (!cfg || !fw) return false;
    if (S.pending >= 6) { S.skipped++; return false; }
    S.pending++; S.sampled++;
    const isFrame = typeof VideoFrame !== 'undefined' && src instanceof VideoFrame;
    const img = isFrame ? src.clone() : src;
    let p;
    if (cfg.mode === 'grid') p = createImageBitmap(img, { resizeWidth: 64, resizeHeight: 36 });
    else {
      const k = fw / cfg.width; const sw = Math.max(1, Math.round(CELLS * cfg.cell * k)), sh = Math.max(1, Math.round(cfg.cellH * k));
      p = createImageBitmap(img, 0, 0, Math.min(sw, fw), Math.min(sh, fh), { resizeWidth: W, resizeHeight: 1, resizeQuality: 'medium' });
    }
    p.then((bm) => {
      if (cfg.mode === 'grid') {
        if (!grid) grid = new OffscreenCanvas(64, 36).getContext('2d', { willReadFrequently: true });
        grid.drawImage(bm, 0, 0); const d = grid.getImageData(0, 0, 64, 36).data;
        let changed = 0, maxDelta = 0, green = 0;
        if (S.prev) for (let i = 0; i < d.length; i += 4) {
          const dl = Math.max(Math.abs(d[i] - S.prev[i]), Math.abs(d[i + 1] - S.prev[i + 1]), Math.abs(d[i + 2] - S.prev[i + 2]));
          if (dl > 24) changed++; if (dl > maxDelta) maxDelta = dl;
          const g = d[i + 1] > d[i] + 40 && d[i + 1] > d[i + 2] + 40, pg = S.prev[i + 1] > S.prev[i] + 40 && S.prev[i + 1] > S.prev[i + 2] + 40;
          if (g && !pg) green++;
        }
        S.prev = new Uint8ClampedArray(d);
        S.out.push({ t, changed, maxDelta, green });
      } else {
        if (!strip) strip = new OffscreenCanvas(W, 1).getContext('2d', { willReadFrequently: true });
        strip.drawImage(bm, 0, 0); const r = decode(strip.getImageData(0, 0, W, 1).data);
        if (r.e) S.errors++;
        S.out.push(r.e ? (r.l && S.diag++ < 40 ? { t, e: r.e, l: r.l } : { t, e: r.e }) : { t, v: r.v });
      }
      bm.close();
    }).catch((e) => { S.out.push({ t, e: 'readback: ' + e.message }); }).finally(() => { if (isFrame) img.close(); S.pending--; });
    if (S.out.length > 20000) S.out.splice(0, 10000);
    return true;
  };
  for (const C of [self.OffscreenCanvasRenderingContext2D, self.CanvasRenderingContext2D]) {
    if (!C) continue;
    const orig = C.prototype.drawImage;
    C.prototype.drawImage = function (src) {
      if (S.cfg && typeof VideoFrame !== 'undefined' && src instanceof VideoFrame) { S.painted++; S.sample(src, src.displayWidth, src.displayHeight, now()); }
      return orig.apply(this, arguments);
    };
  }
})();`;

/** Page-side: sample the <video> element on requestVideoFrameCallback while a cfg is set. */
export const VIDEO_SAMPLER_SOURCE = `(() => {
  const S = self.__seStamp; if (!S || S.videoLoop) return;
  S.videoLoop = true;
  const tick = () => {
    const v = document.getElementById('video');
    if (!S.cfg || !v || typeof v.requestVideoFrameCallback !== 'function') { setTimeout(tick, 200); return; }
    const vis = getComputedStyle(v).display !== 'none' && v.videoWidth > 0;
    if (!vis) { setTimeout(tick, 200); return; }
    v.requestVideoFrameCallback((ts, meta) => {
      if (S.cfg) S.sample(v, v.videoWidth, v.videoHeight, performance.timeOrigin + (meta.expectedDisplayTime || ts));
      tick();
    });
  };
  tick();
})();`;
