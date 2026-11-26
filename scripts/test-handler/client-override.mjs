// Serves local copies of client page files in place of the ones the host embeds, so a page change
// can be tried against a host binary built before it. Loaded ahead of the handler:
//
//   node --import ./scripts/test-handler/client-override.mjs scripts/test-handler/handler.mjs --exe ... --request ...
//
// Environment:
//   SE_TEST_CLIENT_OVERRIDE      NAME=PATH[;NAME=PATH...], e.g. transform-worker.js=C:\x\transform-worker.js;
//                                PATH may be `host` to keep the host's own file (useful with the prelude)
//   SE_TEST_SLOW_MODE            paint | decode | starve, or a comma list taken in turn by successive
//                                `worker_spin` cases: what the case does with its spin_ms (see SLOW_PRELUDE)
//   SE_TEST_HIDDEN_MODE          freeze: the `hidden` case also freezes the page's lifecycle (as a
//                                background tab on a phone is) and thaws it on restore
//   SE_TEST_NO_SCRIPT_TRANSFORM  1: the page takes the legacy createEncodedStreams transport
//   SE_TEST_WORKER_LOG           a .jsonl file: every worker's counters and the prelude's per-frame rows
//                                (read, submit, output, paint times), appended once a second
//
// Only the body changes: the worker needs the same COEP/CORP headers as the page. Nothing here
// dispatches input.
import fs from 'node:fs';
import crypto from 'node:crypto';
import cp, { spawn } from 'node:child_process';
import { syncBuiltinESMExports } from 'node:module';
import { Client, stampMetrics } from './lib/client.mjs';

const OVR = new Map();
for (const part of (process.env.SE_TEST_CLIENT_OVERRIDE || '').split(';').map((s) => s.trim()).filter(Boolean)) {
  const m = /^([\w.-]+)=(.+)$/.exec(part);
  if (!m) { console.error(`SE_TEST_CLIENT_OVERRIDE: bad entry ${part}`); process.exit(2); }
  const body = m[2] === 'host' ? null : fs.readFileSync(m[2]);
  OVR.set(m[1], { path: m[2], body, sha256: body ? crypto.createHash('sha256').update(body).digest('hex') : null, served: 0 });
}
const SLOW_MODES = (process.env.SE_TEST_SLOW_MODE || '').split(',').map((x) => x.trim()).filter(Boolean);
const SLOW_MODE = SLOW_MODES[0] || '';
let slowTurn = 0;
const HIDDEN_MODE = process.env.SE_TEST_HIDDEN_MODE || '';
const LOG = process.env.SE_TEST_WORKER_LOG || '';
const logLine = (o) => { if (LOG) try { fs.appendFileSync(LOG, JSON.stringify({ t: Date.now(), ...o }) + '\n'); } catch (e) { process.stderr.write('[client-override] log: ' + e.message + '\n'); } };
if (SLOW_MODES.some((m) => !['paint', 'decode', 'starve'].includes(m))) { console.error('SE_TEST_SLOW_MODE: paint | decode | starve'); process.exit(2); }
if ((SLOW_MODE || LOG) && !OVR.has('transform-worker.js')) OVR.set('transform-worker.js', { path: 'host', body: null, sha256: null, served: 0 });

// Runs before the worker's own script, in the same global scope. It owns `self.__seSlow`, so the
// handler's own drawImage spin hook stands down and `worker_spin` drives the mode chosen here:
//   paint   every VideoFrame drawImage busy-waits spin ms (installed outermost, at the first output,
//           so the stamp hook times the paint after the wait)
//   decode  the decoder takes at most one chunk per spin ms; decodeQueueSize counts the chunks
//           it has not started, as a real queue does
//   starve  the thread is taken by other work: spin ms busy, 2 ms free, repeated
// It also records, per frame, when it was read, submitted, output and painted.
const SLOW_PRELUDE = (mode) => `'use strict';
(() => {
  const g = self.__seSlow = { spin: 0, spun: 0, mode: ${JSON.stringify(mode)} };
  const now = () => performance.now();
  const rows = []; const recs = new Map();
  const rec = (ts) => { let r = recs.get(ts); if (!r) { r = { ts: ts }; recs.set(ts, r); rows.push(r); if (recs.size > 800) recs.delete(recs.keys().next().value); if (rows.length > 6000) rows.splice(0, 3000); } return r; };
  // A row leaves once it is 2 s old, so the fields filled after its read are in it.
  self.__seDiag = { get mode() { return g.mode; }, drain() { const lim = now() - 2000; let n = 0; while (n < rows.length && ((rows[n].read || rows[n].submit || 0) < lim)) n++; return rows.splice(0, n); } };
  const R0 = ReadableStreamDefaultReader.prototype.read;
  ReadableStreamDefaultReader.prototype.read = function () {
    return R0.apply(this, arguments).then((res) => {
      const v = res && res.value;
      if (v && typeof v.timestamp === 'number' && typeof v.type === 'string' && v.data) {
        const r = rec(v.timestamp >>> 0); r.read = now(); r.type = v.type; r.bytes = v.data.byteLength;
        try { const m = v.getMetadata && v.getMetadata(); if (m && m.receiveTime) r.recv = m.receiveTime; } catch (_) {}
      }
      return res;
    });
  };
  let paintWrapped = false;
  function wrapPaint() {
    if (paintWrapped) return; paintWrapped = true;
    for (const C of [self.OffscreenCanvasRenderingContext2D, self.CanvasRenderingContext2D]) {
      if (!C) continue;
      const d0 = C.prototype.drawImage;
      C.prototype.drawImage = function (src) {
        const isFrame = typeof VideoFrame !== 'undefined' && src instanceof VideoFrame;
        const t = now();
        if (isFrame && g.mode === 'paint' && g.spin > 0) { while (now() - t < g.spin) { /* slow paint */ } g.spun++; }
        const r = isFrame ? recs.get(src.timestamp >>> 0) : null;
        if (r) r.paint = t;
        const x = d0.apply(this, arguments);
        if (r) r.paintEnd = now();
        return x;
      };
    }
  }
  {
    const loop = () => { if (g.spin > 0 && g.mode === 'starve') { const s = now(); while (now() - s < g.spin) { /* other work */ } g.spun++; } setTimeout(loop, g.spin > 0 ? 2 : 50); };
    setTimeout(loop, 50);
  }
  const VD = self.VideoDecoder;
  if (typeof VD === 'function') {
    class SlowDecoder extends VD {
      constructor(init) {
        const out = init.output;
        super({ output: (f) => { wrapPaint(); const r = recs.get(f.timestamp >>> 0); if (r) { r.out = now(); } return out(f); }, error: init.error });
        this.__held = []; this.__next = 0; this.__timer = null;
      }
      get decodeQueueSize() { return super.decodeQueueSize + (this.__held ? this.__held.length : 0); }
      decode(chunk) {
        const r = rec(chunk.timestamp >>> 0); r.submit = now(); r.dq = this.decodeQueueSize;
        if (g.mode === 'decode' && (g.spin > 0 || this.__held.length)) { this.__held.push(chunk); this.__pace(); return; }
        return super.decode(chunk);
      }
      __pace() {
        if (this.__timer !== null) return;
        const step = () => {
          this.__timer = null;
          if (!this.__held.length || this.state !== 'configured') return;
          const t = now();
          if (t < this.__next) { this.__timer = setTimeout(step, this.__next - t); return; }
          const c = this.__held.shift(); g.spun++;
          this.__next = t + (g.spin > 0 ? g.spin : 0);
          try { super.decode(c); } catch (e) { /* the real decoder reports it */ }
          if (this.__held.length) this.__timer = setTimeout(step, Math.max(0, this.__next - now()));
        };
        this.__timer = setTimeout(step, Math.max(0, this.__next - now()));
      }
      reset() { this.__held.length = 0; return super.reset(); }
      close() { this.__held.length = 0; return super.close(); }
    }
    self.VideoDecoder = SlowDecoder;
  }
})();
`;

function prelude() { return SLOW_MODE || LOG ? SLOW_PRELUDE(SLOW_MODE || 'none') : ''; }

// imported with none of the variables set, this module patches nothing
const NO_SCRIPT_TRANSFORM = process.env.SE_TEST_NO_SCRIPT_TRANSFORM === '1';
const ACTIVE = OVR.size > 0 || !!HIDDEN_MODE || NO_SCRIPT_TRANSFORM;
const clients = new Set();
const open0 = Client.prototype.open;
if (ACTIVE) Client.prototype.open = async function (...args) {
  const r = await open0.apply(this, args);
  clients.add(this);
  const p = this.page;
  this.overrides = [...OVR.entries()].map(([name, o]) => ({ name, path: o.path, sha256: o.sha256, served: 0 }));
  if (OVR.size) p.on((d) => {
    if (d.method !== 'Fetch.requestPaused') return;
    const q = d.params;
    const u = new URL(q.request.url);
    const name = u.pathname.replace(/^\//, '');
    const o = OVR.get(name);
    // another interceptor's request (the relay's /whep rewrite) is theirs to answer
    if (!o) return;
    if (q.responseStatusCode === undefined) { p.send('Fetch.continueRequest', { requestId: q.requestId }).catch(() => {}); return; }
    (async () => {
      let body = o.body;
      if (!body) { const b = await p.send('Fetch.getResponseBody', { requestId: q.requestId }); body = Buffer.from(b.body, b.base64Encoded ? 'base64' : 'utf8'); }
      if (name === 'transform-worker.js') body = Buffer.concat([Buffer.from(prelude()), body]);
      const headers = (q.responseHeaders || []).filter((h) => !/^(content-length|content-encoding|etag|last-modified)$/i.test(h.name));
      headers.push({ name: 'Cache-Control', value: 'no-store' });
      await p.send('Fetch.fulfillRequest', { requestId: q.requestId, responseCode: q.responseStatusCode, responseHeaders: headers, body: body.toString('base64') });
      const rec = this.overrides.find((x) => x.name === name); if (rec) rec.served++;
      this.log(`override: served ${name} (${o.path}${name === 'transform-worker.js' && prelude() ? ', prelude ' + (SLOW_MODE || 'log') : ''})`);
    })().catch((e) => { this.log('override ' + name + ': ' + e.message); p.send('Fetch.continueRequest', { requestId: q.requestId }).catch(() => {}); });
  });
  // without RTCRtpScriptTransform the page takes the legacy createEncodedStreams transport
  if (NO_SCRIPT_TRANSFORM) await p.send('Page.addScriptToEvaluateOnNewDocument', { source: 'try { delete self.RTCRtpScriptTransform; } catch (_) {}' });
  if (OVR.size) await p.send('Fetch.enable', { patterns: [...OVR.keys()].map((n) => ({ urlPattern: `*/${n}*`, requestStage: 'Response' })) });
  // phase edges as the handler applies and restores a case, to cut each case's phases out of
  // its stamp window (the handler keeps one result per case name)
  this.__events = [];
  const psend = p.send;
  if (SLOW_MODES.length || LOG) p.send = (method, params, ...rest) => {
    const x = params && params.expression;
    if (method === 'Runtime.evaluate' && typeof x === 'string' && x.indexOf('__seSlow.spin = ') >= 0) {
      if (/__seSlow\.spin = 0[,)]/.test(x)) { if (!this.__events.length || this.__events[this.__events.length - 1].kind !== 'restore') this.__events.push({ kind: 'restore', t: Date.now() }); }
      else {
        const last = this.__events[this.__events.length - 1];
        if (!last || last.kind !== 'apply' || Date.now() - last.t > 2000) { this.__mode = SLOW_MODES[slowTurn++ % Math.max(1, SLOW_MODES.length)] || 'paint'; this.__events.push({ kind: 'apply', t: Date.now(), mode: this.__mode }); logLine({ event: 'apply', mode: this.__mode }); }
        params = { ...params, expression: `(self.__seSlow && (__seSlow.mode = ${JSON.stringify(this.__mode)})), ` + x };
      }
    }
    return psend(method, params, ...rest);
  };
  if (HIDDEN_MODE === 'freeze') {
    const b = this.chrome.browser, send0 = b.send;
    b.send = (method, params, ...rest) => {
      if (method === 'Target.createTarget') { this.__events.push({ kind: 'apply', t: Date.now(), mode: 'hidden' }); logLine({ event: 'apply', mode: 'hidden' }); }
      if (method === 'Target.closeTarget') this.__events.push({ kind: 'restore', t: Date.now() });
      if (method === 'Target.createTarget') p.send('Page.setWebLifecycleState', { state: 'frozen' }).then(() => this.log('lifecycle: frozen'), (e) => this.log('freeze: ' + e.message));
      if (method === 'Target.closeTarget') p.send('Page.setWebLifecycleState', { state: 'active' }).then(() => this.log('lifecycle: active'), (e) => this.log('thaw: ' + e.message));
      return send0(method, params, ...rest);
    };
  }
  if (LOG) {
    const poll = async () => {
      if (!clients.has(this) || p.closed) return;
      for (const sid of [...this.workers.keys()]) {
        const v = await p.send('Runtime.evaluate', { expression: `JSON.stringify({ origin: performance.timeOrigin, now: performance.now(), stats: typeof collectStats === 'function' ? collectStats() : null, rows: self.__seDiag ? self.__seDiag.drain() : null })`, returnByValue: true }, sid, 5000).catch(() => null);
        const s = v && v.result && v.result.value;
        if (s && s !== 'null') { try { const o = JSON.parse(s); if (o.stats || (o.rows && o.rows.length)) fs.appendFileSync(LOG, JSON.stringify({ t: Date.now(), browser: this.browser, ...o }) + '\n'); } catch { /* not ours */ } }
      }
    };
    this.__poll = setInterval(() => { poll().catch(() => {}); }, 1000);
  }
  return r;
};
const collect0 = Client.prototype.collectStamps;
// SE_TEST_WINDOW_SCRIPT: what each stamp window in turn does to the worker, comma separated:
// none | paint:<ms> | decode:<ms> | starve:<ms> | freeze:<ms> (the page frozen that long from the
// middle of the window). For requests with no slow cases of their own (remote_client).
const WINDOW_SCRIPT = (process.env.SE_TEST_WINDOW_SCRIPT || '').split(',').map((x) => x.trim()).filter(Boolean);
async function workerSet(cl, expr) {
  for (const sid of [...cl.workers.keys()]) await cl.page.send('Runtime.evaluate', { expression: expr, returnByValue: true }, sid).catch(() => {});
}
if (LOG) Client.prototype.collectStamps = async function (...args) {
  const idx = this.__windowIdx = (this.__windowIdx === undefined ? 0 : this.__windowIdx + 1);
  const act = WINDOW_SCRIPT[idx] || 'none';
  const [kind, arg] = act.split(':');
  const n = Number(arg) || 0;
  let thaw = null;
  if (kind === 'paint' || kind === 'decode' || kind === 'starve') {
    await workerSet(this, `self.__seSlow && (__seSlow.mode = ${JSON.stringify(kind)}, __seSlow.spin = ${n}, __seSlow.spun = 0)`);
    this.__events.push({ kind: 'apply', t: Date.now(), mode: act });
  } else if (kind === 'freeze') {
    const ms = Number(args[1]) || 20000;
    thaw = (async () => {
      await new Promise((r) => setTimeout(r, ms / 2));
      this.__events.push({ kind: 'apply', t: Date.now(), mode: act });
      await this.page.send('Page.setWebLifecycleState', { state: 'frozen' }).catch((e) => this.log('freeze: ' + e.message));
      await new Promise((r) => setTimeout(r, n || 3000));
      await this.page.send('Page.setWebLifecycleState', { state: 'active' }).catch((e) => this.log('thaw: ' + e.message));
      this.__events.push({ kind: 'restore', t: Date.now() });
    })();
  }
  if (act !== 'none') logLine({ event: 'window', idx, act });
  const t0 = Date.now();
  const got = await collect0.apply(this, args);
  if (thaw) await thaw;
  if (kind === 'paint' || kind === 'decode' || kind === 'starve') {
    await workerSet(this, 'self.__seSlow && (__seSlow.spin = 0)');
    this.__events.push({ kind: 'restore', t: Date.now() });
  }
  const t1 = Date.now();
  const ev = (this.__events || []).filter((e) => e.t >= t0 && e.t <= t1);
  const a = ev.find((e) => e.kind === 'apply'), b = ev.find((e) => e.kind === 'restore' && a && e.t > a.t);
  if (LOG && got && got.samples) try {
    const off = Client.refOffsetMs || 0;
    const ph = (from, to) => { const m = stampMetrics(got.samples, { from: from + off, to: to + off, offsetMs: off }); return { latencyMs: m.latencyMs, paintedFps: m.paintedFps, newStamps: m.newStamps, decodeErrors: m.decodeErrors, longestGapMs: m.longestGapMs, gapsOver100ms: m.gapsOver100ms }; };
    const w = a && b ? { mode: a.mode, baseline: ph(t0, a.t), slow: ph(a.t, b.t), recovery: ph(b.t, t1), recoveryLast5: ph(Math.max(b.t, t1 - 5000), t1), edges: { t0, apply: a.t, restore: b.t, t1 } } : { mode: 'none', all: ph(t0, t1), edges: { t0, t1 } };
    logLine({ window: w, browser: this.browser });
  } catch (e) { process.stderr.write("[client-override] window: " + e.message + '\n'); }
  return got;
};
const close0 = Client.prototype.close;
if (ACTIVE) Client.prototype.close = async function (...args) {
  clients.delete(this);
  if (this.__poll) clearInterval(this.__poll);
  if (this.overrides && this.overrides.length) {
    for (const o of this.overrides) if (!o.served) this.log(`override: ${o.name} was never requested by the page`);
    process.stderr.write(`[client-override] ${JSON.stringify(this.overrides)}\n`);
  }
  return close0.apply(this, args);
};

// SE_TEST_DISPLAY_GATE: a script that waits until a minute has passed since the last virtual display
// add, removal or mode change on this machine and records a new one. Called before every join,
// leave and refresh-rate edit; the record is moved forward again once the display has actually
// changed, because a join's display appears seconds after its gate.
const GATE = process.env.SE_TEST_DISPLAY_GATE || '';
const GATE_FILE = process.env.SE_TEST_DISPLAY_GATE_FILE || '';
function gate(label) {
  if (!GATE) return Promise.resolve();
  return new Promise((resolve) => {
    const c = spawn('bash', [GATE, label], { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = ''; c.stdout.on('data', (b) => { out += b; }); c.stderr.on('data', (b) => { out += b; });
    c.on('exit', () => { process.stderr.write(`[display-gate] ${out.trim()}\n`); resolve(); });
    c.on('error', (e) => { process.stderr.write(`[display-gate] ${e.message}\n`); resolve(); });
  });
}
function restamp(label) {
  if (!GATE_FILE) return;
  try {
    const last = Number(fs.readFileSync(GATE_FILE, 'utf8').replace(/\D/g, '')) || 0;
    if (Date.now() > last) fs.writeFileSync(GATE_FILE, String(Date.now()));
    process.stderr.write(`[display-gate] restamped after ${label}\n`);
  } catch (e) { process.stderr.write(`[display-gate] restamp: ${e.message}\n`); }
}
if (GATE) {
  const join0 = Client.prototype.join;
  // the handler retries a join that painted nothing, and the host may keep that display for its
  // disconnect grace, so the retry's spacing from the removal is unknowable: stop the run instead
  Client.prototype.join = async function (...args) {
    if (this.__lastJoin && this.__lastJoin.firstPictureMs == null) {
      process.stderr.write('[display-gate] refused: a rejoin after a join that painted nothing; stopping this run\n');
      throw new Error('client-override: rejoin refused (display spacing after a failed join cannot be guaranteed)');
    }
    await gate('client-resync join');
    this.__joined = true;
    try { this.__lastJoin = await join0.apply(this, args); return this.__lastJoin; } finally { restamp('join'); }
  };
  const open1 = Client.prototype.open;
  Client.prototype.open = async function (...args) {
    const r = await open1.apply(this, args);
    const p = this.page, send1 = p.send;
    p.send = async (method, params, ...rest) => {
      if (method === 'Page.navigate' && this.__joined && params && params.url === 'about:blank') {
        await gate('client-resync leave');
        this.__joined = false;
        const x = await send1(method, params, ...rest);
        setTimeout(() => restamp('leave'), 8000);
        return x;
      }
      return send1(method, params, ...rest);
    };
    return r;
  };
  // closed while joined: the host removes the display after its disconnect grace
  const close1 = Client.prototype.close;
  Client.prototype.close = async function (...args) {
    if (this.__joined) { await gate('client-resync close while joined'); this.__joined = false; setTimeout(() => restamp('close while joined'), 20000); }
    return close1.apply(this, args);
  };
  const exec0 = cp.execFile;
  cp.execFile = function (file, args, ...rest) {
    if (Array.isArray(args) && args[0] === 'devices' && args[1] === 'set' && args.includes('--refresh-rate')) {
      const cb = rest[rest.length - 1];
      if (typeof cb === 'function') {
        gate('client-resync refresh-rate edit').then(() => exec0.call(cp, file, args, ...rest.slice(0, -1), (...a) => { setTimeout(() => restamp('mode change'), 6000); cb(...a); }));
        return { kill() {}, on() {} };
      }
    }
    return exec0.call(cp, file, args, ...rest);
  };
  syncBuiltinESMExports();
}
