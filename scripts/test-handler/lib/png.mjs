// Minimal PNG decoder for Chrome's screencast frames (8-bit RGB or RGBA, non-interlaced).
// `maxRows` stops unfiltering early: the clock stamp sits near the top of the picture, and a
// row's filter depends only on the row above it.
import zlib from 'node:zlib';

export function decodePng(buf, maxRows = Infinity) {
  if (buf.readUInt32BE(0) !== 0x89504e47) throw new Error('not a PNG');
  let off = 8, w = 0, h = 0, colorType = 0, depth = 0, interlace = 0;
  const idat = [];
  while (off < buf.length) {
    const len = buf.readUInt32BE(off); const type = buf.toString('ascii', off + 4, off + 8);
    const data = buf.subarray(off + 8, off + 8 + len);
    if (type === 'IHDR') { w = data.readUInt32BE(0); h = data.readUInt32BE(4); depth = data[8]; colorType = data[9]; interlace = data[12]; }
    else if (type === 'IDAT') idat.push(data);
    else if (type === 'IEND') break;
    off += 12 + len;
  }
  if (depth !== 8 || interlace !== 0 || (colorType !== 2 && colorType !== 6)) throw new Error(`unsupported PNG ${depth}/${colorType}/${interlace}`);
  const bpp = colorType === 6 ? 4 : 3;
  const raw = zlib.inflateSync(Buffer.concat(idat));
  const stride = w * bpp; const rows = Math.min(h, maxRows);
  const out = Buffer.alloc(stride * rows);
  for (let y = 0; y < rows; y++) {
    const ft = raw[y * (stride + 1)]; const src = y * (stride + 1) + 1; const dst = y * stride; const up = dst - stride;
    for (let x = 0; x < stride; x++) {
      const a = x >= bpp ? out[dst + x - bpp] : 0; const b = y > 0 ? out[up + x] : 0; const c = x >= bpp && y > 0 ? out[up + x - bpp] : 0;
      let v = raw[src + x];
      switch (ft) {
        case 1: v += a; break;
        case 2: v += b; break;
        case 3: v += (a + b) >> 1; break;
        case 4: { const p = a + b - c, pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c); v += pa <= pb && pa <= pc ? a : pb <= pc ? b : c; break; }
        default: break;
      }
      out[dst + x] = v & 0xff;
    }
  }
  return { width: w, height: h, rows, bpp, data: out };
}

/** Width and height from the IHDR chunk, without inflating anything. */
export function pngSize(buf) { return { width: buf.readUInt32BE(16), height: buf.readUInt32BE(20) }; }

export function luma(img, x, y) {
  x = Math.max(0, Math.min(img.width - 1, Math.round(x))); y = Math.max(0, Math.min(img.rows - 1, Math.round(y)));
  const i = (y * img.width + x) * img.bpp;
  return 0.299 * img.data[i] + 0.587 * img.data[i + 1] + 0.114 * img.data[i + 2];
}

export function rgb(img, x, y) {
  const i = (Math.round(y) * img.width + Math.round(x)) * img.bpp;
  return [img.data[i], img.data[i + 1], img.data[i + 2]];
}
