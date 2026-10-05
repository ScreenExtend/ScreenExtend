// Decode the clock stamp (stage.ps1's cell row) from a decoded screencast image.
import { luma } from './png.mjs';

export const CELLS = 38;

/** Where the stamp's cells are in image pixels. geom: GEOMETRY_EXPR result; stage: stage.ps1 ready line. */
export function stampLayout(geom, stage, img) {
  if (!geom || !geom.live || !stage) return null;
  const k = img.width / geom.vw;                    // image px per CSS px
  // The picture is the display, letterboxed into the element. A transferred canvas reports
  // its placeholder size (300x150), not the frame's, so the display's own aspect is used.
  let x = geom.x, y = geom.y, w = geom.w;
  if (geom.rw && stage.height) {
    const s = Math.min(geom.rw / stage.width, geom.rh / stage.height);
    w = stage.width * s; x = geom.rx + (geom.rw - w) / 2; y = geom.ry + (geom.rh - stage.height * s) / 2;
  }
  const d = w / stage.width;                        // CSS px per display px
  const cw = stage.cell * d * k, ch = stage.cellH * d * k;
  return { x0: x * k, y0: y * k, cw, ch, maxRow: Math.ceil(y * k + ch * 1.5) + 2 };
}

function cellLuma(img, L, i, dy = 0) {
  const cx = L.x0 + (i + 0.5) * L.cw, cy = L.y0 + (0.5 + dy) * L.ch;
  const r = Math.max(0, Math.min(2, Math.floor(Math.min(L.cw, L.ch) / 4)));
  let s = 0, n = 0;
  for (let y = -r; y <= r; y++) for (let x = -r; x <= r; x++) { s += luma(img, cx + x, cy + y); n++; }
  return s / n;
}

/** Returns { value } (24-bit ms) or { error }. */
export function decodeStamp(img, L) {
  if (!L) return { error: 'no layout' };
  if (L.cw < 2.5) return { error: `cells too small (${L.cw.toFixed(1)} px)` };
  for (const dy of [0, -0.2, 0.2]) {
    const v = []; for (let i = 0; i < CELLS; i++) v.push(cellLuma(img, L, i, dy));
    const white = (v[0] + v[2] + v[37]) / 3, black = (v[1] + v[3] + v[36]) / 3;
    if (white - black < 60) continue;
    const th = (white + black) / 2;
    const bits = v.map((x) => (x > th ? 1 : 0));
    if (bits[0] !== 1 || bits[1] !== 0 || bits[2] !== 1 || bits[3] !== 0 || bits[36] !== 0 || bits[37] !== 1) continue;
    let val = 0; for (let i = 0; i < 24; i++) val = val * 2 + bits[4 + i];
    let chk = 0; for (let i = 0; i < 8; i++) chk = chk * 2 + bits[28 + i];
    const sum = (((val >> 16) & 0xff) + ((val >> 8) & 0xff) + (val & 0xff)) & 0xff;
    if ((sum ^ 0x5a) !== chk) return { error: 'checksum' };
    return { value: val, contrast: Math.round(white - black) };
  }
  return { error: 'guard' };
}

/** Unwrap a 24-bit ms stamp against a full epoch-ms reference (the frame's own time). */
export function unwrap(stamp24, refMs) {
  const M = 1 << 24; const refLow = Math.floor(refMs) % M;
  let d = refLow - stamp24; if (d < -M / 2) d += M; if (d > M / 2) d -= M;
  return Math.floor(refMs) - d;
}
