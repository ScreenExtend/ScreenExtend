// Builds test variants of the shipped client video worker for the "where does the ~10 ms go"
// experiment (run-mac.mjs --tick --client-override transform-worker.js=<variant>). The shipped
// file is never edited: each variant is the shipped source plus a few anchored insertions, and
// the build fails loudly if an anchor has moved.
//
//   node scripts/e2e-headless/tick-variants.mjs <out-dir> [path/to/transform-worker.js]
//
//   a          read + timestamp only: no decode, no paint (frames counted as rendered so the page's
//              watchdog stays quiet)
//   b          read + WebCodecs decode, output closed at once, no paint
//   track      shipped, plus a timeline mark when a frame goes to the VideoTrackGenerator
//              (use with --url-param present=track)
//   ibr        OffscreenCanvas 'bitmaprenderer': createImageBitmap + transferFromImageBitmap
//   gl         OffscreenCanvas WebGL: texImage2D(VideoFrame) + one quad
//   main       decoded frames transferred to the page and drawn on a main-thread canvas 2D
//   maindesync the same with getContext('2d', { desynchronized: true })
import fs from 'node:fs';
import path from 'node:path';

const [outDir, srcPath = 'src-tauri/src/streamer/static/transform-worker.js'] = process.argv.slice(2);
if (!outDir) { console.error('usage: tick-variants.mjs <out-dir> [transform-worker.js]'); process.exit(64); }
const src = fs.readFileSync(srcPath, 'utf8');

function at(s, anchor, insert, where) {
  const i = s.indexOf(anchor);
  if (i < 0 || s.indexOf(anchor, i + 1) >= 0) throw new Error('anchor not unique/found: ' + anchor);
  return where === 'before' ? s.slice(0, i) + insert + s.slice(i) : s.slice(0, i + anchor.length) + insert + s.slice(i + anchor.length);
}

const GL = `
// ---- tick experiment: WebGL presenter
let seGl = null;
function seGlInit(c) {
  const gl = c.getContext('webgl2', { alpha: false, antialias: false, depth: false }) || c.getContext('webgl', { alpha: false, antialias: false, depth: false });
  if (!gl) return null;
  const sh = function (t, s) { const o = gl.createShader(t); gl.shaderSource(o, s); gl.compileShader(o); return o; };
  const p = gl.createProgram();
  gl.attachShader(p, sh(gl.VERTEX_SHADER, 'attribute vec2 a; varying vec2 v; void main(){ v = vec2((a.x+1.0)*0.5, (1.0-a.y)*0.5); gl_Position = vec4(a,0.0,1.0); }'));
  gl.attachShader(p, sh(gl.FRAGMENT_SHADER, 'precision mediump float; varying vec2 v; uniform sampler2D t; void main(){ gl_FragColor = texture2D(t, v); }'));
  gl.linkProgram(p); gl.useProgram(p);
  const b = gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER, b);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
  const loc = gl.getAttribLocation(p, 'a'); gl.enableVertexAttribArray(loc); gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
  const tex = gl.createTexture(); gl.bindTexture(gl.TEXTURE_2D, tex);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR); gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE); gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  seGl = { gl: gl, tex: tex };
  return gl;
}
function seGlDraw(frame) {
  const gl = seGl.gl;
  gl.viewport(0, 0, canvas.width, canvas.height);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame);
  gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
}
`;

function build(v) {
  let s = at(src, "'use strict';", `\nconst SE_VARIANT = '${v}';\n`);
  // a: read only.
  s = at(s, "if (type === 'key') keyframesIn++;", `
      if (SE_VARIANT === 'a') {
        framesRendered++;
        if (!renderedOnce) { renderedOnce = true; self.postMessage({ type: 'rendered' }); }
        if (type === 'key') forwardKeyframe(encodedFrame);
        continue;
      }`);
  // b: decode, no paint.
  s = at(s, 'function presentFrame(frame) {', "\n  if (SE_VARIANT === 'b') return PRESENT.painted;");
  // track: mark the hand-off to the generator (the frame is detached after write()).
  s = at(s, 'genWriter.write(frame).then(null, note);', "const seTs = frame.timestamp; ", 'before');
  s = at(s, 'genWriter.write(frame).then(null, note);', ' if (self.__seMark) self.__seMark(seTs);');
  // ibr / gl contexts.
  s = at(s, "ctx = canvas.getContext('2d', { alpha: false });", "ctx = SE_VARIANT === 'ibr' ? canvas.getContext('bitmaprenderer') : SE_VARIANT === 'gl' ? seGlInit(canvas) : ", 'before');
  s = s.replace("ctx = SE_VARIANT === 'ibr' ? canvas.getContext('bitmaprenderer') : SE_VARIANT === 'gl' ? seGlInit(canvas) : ctx = canvas.getContext('2d', { alpha: false });",
    "ctx = SE_VARIANT === 'ibr' ? canvas.getContext('bitmaprenderer') : SE_VARIANT === 'gl' ? seGlInit(canvas) : canvas.getContext('2d', { alpha: false });");
  s = at(s, '  ctx.drawImage(frame, 0, 0);', `  if (SE_VARIANT === 'ibr') {
    const seTs2 = frame.timestamp;
    createImageBitmap(frame).then(function (bmp) { try { frame.close(); } catch (_) {} ctx.transferFromImageBitmap(bmp); if (self.__seMark) self.__seMark(seTs2); }, function () { try { frame.close(); } catch (_) {} });
    return PRESENT.handedOff;
  }
  if (SE_VARIANT === 'gl') { seGlDraw(frame); if (self.__seMark) self.__seMark(frame.timestamp); return PRESENT.painted; }
  if (SE_VARIANT === 'main' || SE_VARIANT === 'maindesync') {
    self.postMessage({ type: 'sevf', frame: frame, ts: frame.timestamp, desync: SE_VARIANT === 'maindesync' }, [frame]);
    return PRESENT.handedOff;
  }
`, 'before');
  return s + GL;
}

fs.mkdirSync(outDir, { recursive: true });
for (const v of ['a', 'b', 'track', 'ibr', 'gl', 'main', 'maindesync']) {
  const f = path.join(outDir, `transform-worker.${v}.js`);
  fs.writeFileSync(f, build(v));
  console.log(f);
}
