// Loopback UDP relay for one browser's media, with impairment and no admin rights.
// The WHEP answer's host candidates are rewritten to the relay and the browser's own candidates
// are stripped from the offer and every trickle PATCH, so the only pair that can form goes
// through here. HTTPS signalling stays direct.
//
// Per direction (down = host to browser, up = browser to host), in this order:
//   stall    every `every_s` s hold everything for `ms`, then release in arrival order
//   loss     Bernoulli (`loss`) or Gilbert-Elliott (`burst: {p_enter, p_exit, loss}`)
//   delay    `delay_ms` plus uniform jitter of +-`jitter_ms`; order kept unless `reorder`
//   rate     token bucket (`rate_kbps`, `bucket_bytes`) serving a FIFO drop-tail queue of
//            `queue_bytes` (default `queue_ms` at the rate, and `queue_ms` defaults to 100)
// `rate_of_baseline: f` caps at f times the direction's input rate since `markBaseline()`;
// the first setting that asks measures it, every later one reuses that number.
// `schedule: [{ at_s, down, up }]` replaces settings that long after `arm()`.
// Times are on the precise system clock (lib/clock.mjs); node's hrtime is QPC on Windows.
import { Worker } from 'node:worker_threads';
import { preciseNow } from './clock.mjs';

const WORKER = String.raw`
const { parentPort, workerData } = require('node:worker_threads');
const dgram = require('node:dgram');
const K = workerData.K;
const now = () => Number(process.hrtime.bigint()) / 1e6 + K;
const HOST = workerData.hostIp, BIND = workerData.bindIp;
const BUF = 4 * 1024 * 1024;
const DEF = { loss: 0, burst: null, delay_ms: 0, jitter_ms: 0, reorder: false, rate_kbps: 0, bucket_bytes: 16000, queue_bytes: 0, queue_ms: 0, rate_of_baseline: 0, stall: null };
const mkTot = () => ({ in: 0, inBytes: 0, sent: 0, sentBytes: 0, lossDrops: 0, queueDrops: 0, held: 0, heldBytes: 0, stalls: 0, sendErrors: 0, maxQueueBytes: 0, maxDelayLineBytes: 0 });
const dirs = {};
for (const d of ['down', 'up']) dirs[d] = { cfg: Object.assign({}, DEF), tot: mkTot(), sec: mkTot(), ge: false, holding: false, held: [], line: [], lastDue: 0, q: [], qBytes: 0, lineBytes: 0, tokens: 16000, tokAt: now(), late: [], stallTimer: null };
// input rate per direction since markBaseline(), measured once on first use
let base = null;
function baselineKbps(d) {
  if (!base) return null;
  if (base.kbps[d] == null) { const ms = now() - base.at; base.kbps[d] = ms > 0 ? (dirs[d].tot.inBytes - base.inBytes[d]) * 8 / ms : 0; base.until[d] = Math.round(now()); }
  return base.kbps[d];
}
function setCfg(d, c) {
  const D = dirs[d]; const old = D.cfg; D.cfg = Object.assign({}, DEF, c || {});
  if (D.cfg.rate_of_baseline) {
    const b = baselineKbps(d);
    D.cfg.baseline_kbps = b == null ? null : Math.round(b);
    if (b) D.cfg.rate_kbps = Math.max(50, Math.round(b * D.cfg.rate_of_baseline));
    else D.cfg.rate_error = b == null ? 'no baseline marked' : 'nothing arrived during the baseline';
  }
  if (D.cfg.rate_kbps && !D.cfg.queue_bytes) D.cfg.queue_bytes = Math.max(3000, Math.round(D.cfg.rate_kbps * 1000 / 8 * (D.cfg.queue_ms || 100) / 1000));
  D.tokens = Math.min(D.tokens, D.cfg.bucket_bytes);
  if (D.stallTimer) { clearInterval(D.stallTimer); D.stallTimer = null; }
  if (D.cfg.stall) D.stallTimer = setInterval(() => { D.tot.stalls++; D.sec.stalls++; D.holding = true; setTimeout(() => releaseHeld(d), D.cfg.stall.ms); }, D.cfg.stall.every_s * 1000);
  if (old.stall && !D.cfg.stall && D.holding) releaseHeld(d);
}
setCfg('down', workerData.cfg.down); setCfg('up', workerData.cfg.up);
function releaseHeld(d) { const D = dirs[d]; D.holding = false; const h = D.held; D.held = []; for (const p of h) admit(d, p); pump(); }
// Frame tap (down): per RTP stream, the packets of each RTP timestamp as they are sent.
const tap = { on: false, frames: [], cur: new Map(), streams: new Map() };
function tapPacket(m, t) {
  if (m.length < 12 || (m[0] & 0xC0) !== 0x80) return;
  const b1 = m[1]; if (b1 >= 192 && b1 <= 223) return; // RTCP
  const ssrc = m.readUInt32BE(8), ts = m.readUInt32BE(4), marker = b1 >> 7, pt = b1 & 0x7f;
  let s = tap.streams.get(ssrc); if (!s) { s = { ssrc, pt, packets: 0, bytes: 0, seqGaps: 0, reordered: 0, last: -1 }; tap.streams.set(ssrc, s); }
  s.packets++; s.bytes += m.length;
  // holes here were lost before the relay
  const seq = m.readUInt16BE(2);
  if (s.last >= 0) { const d = (seq - s.last + 65536) & 0xffff; if (d > 0 && d < 32768) { s.seqGaps += d - 1; s.last = seq; } else if (d !== 0) s.reordered++; } else s.last = seq;
  if (!tap.on) return;
  let f = tap.cur.get(ssrc);
  if (!f || f.rtp !== ts) { f = { ssrc, pt, rtp: ts, firstAt: t, lastAt: t, n: 0, bytes: 0, marker: false }; tap.cur.set(ssrc, f); }
  f.n++; f.bytes += m.length; f.lastAt = t;
  if (marker) { f.marker = true; if (tap.frames.length < 200000) tap.frames.push(f); tap.cur.delete(ssrc); }
}
function geLoss(D) {
  const b = D.cfg.burst;
  if (b) { if (D.ge) { if (Math.random() < b.p_exit) D.ge = false; } else if (Math.random() < b.p_enter) D.ge = true; return D.ge && Math.random() < (b.loss == null ? 1 : b.loss); }
  return D.cfg.loss > 0 && Math.random() < D.cfg.loss;
}
// p = { msg, send(msg), t }
function admit(d, p) {
  const D = dirs[d];
  if (geLoss(D)) { D.tot.lossDrops++; D.sec.lossDrops++; return; }
  const c = D.cfg;
  if (!c.delay_ms && !c.jitter_ms && !c.rate_kbps && !D.line.length && !D.q.length) { send(d, p, null); return; }
  let due = now() + (c.delay_ms || 0) + (c.jitter_ms ? (Math.random() * 2 - 1) * c.jitter_ms : 0);
  if (!c.reorder && due < D.lastDue) due = D.lastDue;
  D.lastDue = due; p.due = due;
  if (c.reorder) { let i = D.line.length; while (i > 0 && D.line[i - 1].due > due) i--; D.line.splice(i, 0, p); } else D.line.push(p);
  D.lineBytes += p.msg.length; if (D.lineBytes > D.tot.maxDelayLineBytes) D.tot.maxDelayLineBytes = D.lineBytes;
}
function send(d, p, intended) {
  const D = dirs[d]; const t = now();
  if (intended != null) { const l = t - intended; if (D.late.length < 100000) D.late.push(l); }
  if (d === 'down') tapPacket(p.msg, t);
  D.tot.sent++; D.tot.sentBytes += p.msg.length; D.sec.sent++; D.sec.sentBytes += p.msg.length;
  p.send(p.msg);
}
let timer = null, spinning = false, target = null; const wake = [];
function pump() {
  timer = null; spinning = false;
  const t = now(); let next = Infinity;
  if (target != null && t >= target) { if (wake.length < 100000) wake.push(t - target); target = null; }
  for (const d of ['down', 'up']) {
    const D = dirs[d]; const c = D.cfg;
    while (D.line.length && D.line[0].due <= t) {
      const p = D.line.shift(); D.lineBytes -= p.msg.length;
      if (!c.rate_kbps) { send(d, p, p.due); continue; }
      if (D.qBytes + p.msg.length > c.queue_bytes) { D.tot.queueDrops++; D.sec.queueDrops++; continue; }
      p.qAt = Math.max(p.due, t); D.q.push(p); D.qBytes += p.msg.length; if (D.qBytes > D.tot.maxQueueBytes) D.tot.maxQueueBytes = D.qBytes;
    }
    if (D.line.length) next = Math.min(next, D.line[0].due);
    if (D.q.length) {
      if (!c.rate_kbps) { while (D.q.length) { const p = D.q.shift(); D.qBytes -= p.msg.length; send(d, p, p.qAt); } }
      else {
        const rate = c.rate_kbps * 1000 / 8 / 1000; // bytes per ms
        D.tokens = Math.min(c.bucket_bytes, D.tokens + (t - D.tokAt) * rate); D.tokAt = t;
        while (D.q.length && D.tokens >= D.q[0].msg.length) {
          const p = D.q.shift(); D.qBytes -= p.msg.length; D.tokens -= p.msg.length;
          send(d, p, null);
        }
        if (D.q.length) next = Math.min(next, t + (D.q[0].msg.length - D.tokens) / rate);
      }
    }
  }
  schedule(next);
}
function schedule(next) {
  if (next === Infinity || timer || spinning) return;
  const w = next - now(); target = next;
  // libuv timers follow the system timer period (up to 15.6 ms late); spin the last stretch.
  if (w > 20) timer = setTimeout(pump, Math.floor(w - 16));
  else { spinning = true; setImmediate(pump); }
}
function enqueue(d, msg, sendFn) {
  const D = dirs[d];
  D.tot.in++; D.tot.inBytes += msg.length; D.sec.in++; D.sec.inBytes += msg.length;
  const p = { msg, send: sendFn, t: now() };
  if (D.holding) { D.held.push(p); D.tot.held++; D.tot.heldBytes += msg.length; D.sec.held++; return; }
  admit(d, p); pump();
}
const socks = []; const ports = {}; const buffers = [];
function bufs(sock, label) { const r = { label }; try { sock.setRecvBufferSize(BUF); sock.setSendBufferSize(BUF); r.recv = sock.getRecvBufferSize(); r.send = sock.getSendBufferSize(); } catch (e) { r.error = e.message; } buffers.push(r); }
async function relayFor(hostPort, hostIp) {
  const key = (hostIp || HOST) + ':' + hostPort;
  if (ports[key]) return ports[key];
  const inbound = dgram.createSocket('udp4'); socks.push(inbound);
  const peers = new Map();
  inbound.on('message', (msg, rinfo) => {
    const k = rinfo.address + ':' + rinfo.port;
    let out = peers.get(k);
    if (!out) {
      out = dgram.createSocket('udp4'); socks.push(out);
      const toBrowser = (m) => inbound.send(m, rinfo.port, rinfo.address, (e) => { if (e) dirs.down.tot.sendErrors++; });
      out.on('message', (m) => enqueue('down', m, toBrowser));
      out.on('error', () => {});
      out.bind(0, () => bufs(out, 'to-host:' + key));
      peers.set(k, out);
    }
    const o = out;
    enqueue('up', msg, (m) => o.send(m, hostPort, hostIp || HOST, (e) => { if (e) dirs.up.tot.sendErrors++; }));
  });
  inbound.on('error', () => {});
  const local = await new Promise((res, rej) => { inbound.once('error', rej); inbound.bind(0, BIND, () => res(inbound.address().port)); });
  bufs(inbound, 'from-browser:' + local);
  ports[key] = local;
  return local;
}
// Per-second rows.
const seconds = []; let t0 = now(); let armedAt = null; const sched = []; let schedTimers = [];
setInterval(() => {
  const row = { t: Math.round(now()), s: armedAt ? Math.round((now() - armedAt) / 100) / 10 : null };
  for (const d of ['down', 'up']) { const D = dirs[d]; row[d] = Object.assign({}, D.sec, { queueBytes: D.qBytes, queueMs: D.cfg.rate_kbps ? Math.round(D.qBytes * 8 / D.cfg.rate_kbps) : 0, delayLineBytes: D.lineBytes, holding: D.holding }); D.sec = mkTot(); }
  if (seconds.length < 20000) seconds.push(row);
}, 1000);
function pct(a) { const s = a.slice().sort((x, y) => x - y); const q = (p) => (s.length ? Math.round(s[Math.min(s.length - 1, Math.floor(p * s.length))] * 100) / 100 : null); return { n: s.length, p50: q(0.5), p90: q(0.9), p99: q(0.99), max: s.length ? Math.round(s[s.length - 1] * 100) / 100 : null }; }
parentPort.on('message', async (m) => {
  let v = null;
  if (m.type === 'port') v = await relayFor(m.hostPort, m.hostIp);
  else if (m.type === 'arm') { armedAt = now(); for (const t of schedTimers) clearTimeout(t); schedTimers = (m.schedule || []).map((ph) => setTimeout(() => { if (ph.down) setCfg('down', ph.down); if (ph.up) setCfg('up', ph.up); sched.push({ at: Math.round(now()), s: ph.at_s, down: dirs.down.cfg, up: dirs.up.cfg }); }, ph.at_s * 1000)); v = armedAt; }
  else if (m.type === 'set') { if (m.down !== undefined) setCfg('down', m.down); if (m.up !== undefined) setCfg('up', m.up); sched.push({ at: Math.round(now()), set: true, down: dirs.down.cfg, up: dirs.up.cfg }); v = { down: dirs.down.cfg, up: dirs.up.cfg }; }
  else if (m.type === 'disarm') { for (const t of schedTimers) clearTimeout(t); schedTimers = []; v = true; }
  else if (m.type === 'mark') { base = { at: now(), inBytes: { down: dirs.down.tot.inBytes, up: dirs.up.tot.inBytes }, kbps: { down: null, up: null }, until: { down: null, up: null } }; v = Math.round(base.at); }
  else if (m.type === 'tapOn') { tap.on = true; tap.frames = []; tap.cur.clear(); v = true; }
  else if (m.type === 'tapOff') { tap.on = false; let best = null; for (const s of tap.streams.values()) if (!best || s.bytes > best.bytes) best = s; v = { videoSsrc: best ? best.ssrc : null, streams: [...tap.streams.values()], frames: tap.frames.filter((f) => best && f.ssrc === best.ssrc).map((f) => ({ rtp: f.rtp, firstAt: f.firstAt, lastAt: f.lastAt, n: f.n, bytes: f.bytes })) }; tap.frames = []; }
  else if (m.type === 'stats') {
    const since = m.since || 0;
    v = { totals: { down: dirs.down.tot, up: dirs.up.tot }, config: { down: dirs.down.cfg, up: dirs.up.cfg }, baseline: base ? { at: Math.round(base.at), until: base.until, kbps: base.kbps } : null, seconds: seconds.filter((r) => r.t >= since), schedule: sched, ports, buffers,
      lateness_ms: { down: pct(dirs.down.late), up: pct(dirs.up.late), wake: pct(wake), note: 'down/up: actual send - due time of each delayed packet; wake: actual - intended run of the scheduler (delay and token bucket)' }, streams: [...tap.streams.values()] };
    if (m.resetLate) { dirs.down.late = []; dirs.up.late = []; wake.length = 0; }
  } else if (m.type === 'close') { for (const d of ['down', 'up']) if (dirs[d].stallTimer) clearInterval(dirs[d].stallTimer); for (const s of socks) { try { s.close(); } catch (_) {} } v = true; }
  parentPort.postMessage({ id: m.id, value: v });
});
`;

export const QUEUE_DISCIPLINE = 'per direction: stall gate (hold everything, release in arrival order) -> Bernoulli or Gilbert-Elliott loss -> fixed delay + uniform jitter (arrival order kept unless reorder) -> FIFO drop-tail queue limited in bytes (default queue_ms, 100 ms, at the rate), served by a token bucket (rate, bucket size; rate_of_baseline scales the input rate measured since the baseline mark)';

export class Relay {
  constructor({ hostIp, bindIp = null, cfg = {} }) {
    // Chrome on Windows binds media sockets to real interfaces and will not send to 127.0.0.1
    this.hostIp = hostIp; this.bindIp = bindIp || hostIp; this.seq = 0; this.waits = new Map(); this.cfg = cfg;
    const K = preciseNow() - Number(process.hrtime.bigint()) / 1e6;
    this.worker = new Worker(WORKER, { eval: true, workerData: { hostIp, bindIp: this.bindIp, K, cfg: { down: cfg.down || {}, up: cfg.up || {} } } });
    this.worker.on('message', (m) => { const w = this.waits.get(m.id); if (w) { this.waits.delete(m.id); w(m.value); } });
    this.worker.on('error', (e) => { this.error = e.message; });
    this.rewrites = { answers: 0, offers: 0, patches: 0, answerCandidates: [], strippedClientCandidates: 0 };
  }
  call(msg, timeout = 8000) {
    const id = ++this.seq;
    return new Promise((res) => { this.waits.set(id, res); this.worker.postMessage({ ...msg, id }); setTimeout(() => { if (this.waits.delete(id)) res(null); }, timeout); });
  }
  portFor(hostPort, hostIp) { return this.call({ type: 'port', hostPort, hostIp }); }
  arm(schedule) { return this.call({ type: 'arm', schedule: schedule || this.cfg.schedule || [] }); }
  /** Resolves to the settings now in effect ({down, up}), with any baseline-relative cap worked out. */
  set(c) { return this.call({ type: 'set', ...c }); }
  /** Start the window a later `rate_of_baseline` measures the input rate over. */
  markBaseline() { return this.call({ type: 'mark' }); }
  /** Drop schedule phases that have not fired yet. */
  disarm() { return this.call({ type: 'disarm' }); }
  stats(since, resetLate = false) { return this.call({ type: 'stats', since, resetLate }); }
  armFrames() { return this.call({ type: 'tapOn' }); }
  async readFrames() { const v = await this.call({ type: 'tapOff' }); return v ? v.frames : null; }
  async stop() { const s = await this.stats(0); await this.call({ type: 'close' }); await this.worker.terminate(); return s; }

  /** The host's UDP host candidates -> <bindIp>:<relay>; every other candidate is dropped. */
  async rewriteAnswer(sdp) {
    const eol = sdp.includes('\r\n') ? '\r\n' : '\n'; const out = []; const cands = [];
    const RX = /^(a=candidate:\S+ \d+ udp \d+ )(\d+\.\d+\.\d+\.\d+) (\d+)( typ host.*)$/i;
    const lines = sdp.split(eol);
    const v4 = lines.filter((l) => RX.test(l));
    const keep = new Set(v4.some((l) => RX.exec(l)[2] === this.hostIp) ? v4.filter((l) => RX.exec(l)[2] === this.hostIp) : v4.slice(0, 1));
    for (const line of lines) {
      if (keep.has(line)) { const m = RX.exec(line); const p = await this.portFor(Number(m[3]), m[2]); out.push(`${m[1]}${this.bindIp} ${p}${m[4]}`); cands.push(`${m[2]}:${m[3]} -> ${this.bindIp}:${p}`); continue; }
      if (/^a=candidate:/i.test(line)) continue;
      out.push(line);
    }
    this.rewrites.answers++; this.rewrites.answerCandidates.push(...cands);
    return out.join(eol);
  }
  /** The browser's candidates out of an offer or a trickle fragment. */
  stripClientCandidates(body) {
    const eol = body.includes('\r\n') ? '\r\n' : '\n'; let n = 0;
    const out = body.split(eol).filter((l) => { if (/^a=candidate:/i.test(l)) { n++; return false; } return true; }).join(eol);
    this.rewrites.strippedClientCandidates += n; return out;
  }

  /** Route one page's media through this relay. Call before the join. */
  async attach(page) {
    this.page = page;
    page.on((d) => {
      if (d.method !== 'Fetch.requestPaused') return;
      const q = d.params;
      (async () => {
        if (!/\/whep/.test(q.request.url)) return page.send('Fetch.continueRequest', { requestId: q.requestId });
        if (q.responseStatusCode === undefined) {
          const raw = q.request.postDataEntries ? Buffer.concat(q.request.postDataEntries.map((e) => Buffer.from(e.bytes || '', 'base64'))).toString('utf8') : (q.request.postData || '');
          if (!raw || !/a=candidate:/i.test(raw)) return page.send('Fetch.continueRequest', { requestId: q.requestId });
          let body = raw;
          // a JSON offer body ({ sdp, ... }) or a bare SDP / sdpfrag
          if (/^\s*\{/.test(raw)) { try { const j = JSON.parse(raw); for (const k of Object.keys(j)) if (typeof j[k] === 'string' && /a=candidate:/i.test(j[k])) j[k] = this.stripClientCandidates(j[k]); body = JSON.stringify(j); } catch { body = raw; } }
          else body = this.stripClientCandidates(raw);
          if (q.request.method === 'PATCH') this.rewrites.patches++; else this.rewrites.offers++;
          return page.send('Fetch.continueRequest', { requestId: q.requestId, postData: Buffer.from(body).toString('base64') });
        }
        if (q.request.method !== 'POST' || !(q.responseStatusCode >= 200 && q.responseStatusCode < 300)) return page.send('Fetch.continueRequest', { requestId: q.requestId });
        const b = await page.send('Fetch.getResponseBody', { requestId: q.requestId });
        const text = b.base64Encoded ? Buffer.from(b.body, 'base64').toString('utf8') : b.body;
        let outText;
        if (/^\s*\{/.test(text)) { const j = JSON.parse(text); for (const k of Object.keys(j)) if (typeof j[k] === 'string' && /a=candidate:/i.test(j[k])) j[k] = await this.rewriteAnswer(j[k]); outText = JSON.stringify(j); }
        else outText = await this.rewriteAnswer(text);
        const headers = (q.responseHeaders || []).filter((h) => !/^content-length$/i.test(h.name));
        await page.send('Fetch.fulfillRequest', { requestId: q.requestId, responseCode: q.responseStatusCode, responseHeaders: headers, body: Buffer.from(outText).toString('base64') });
      })().catch((e) => { this.fetchError = e.message; page.send('Fetch.continueRequest', { requestId: q.requestId }).catch(() => {}); });
    });
    await page.send('Fetch.enable', { patterns: [{ urlPattern: '*/whep*', requestStage: 'Request' }, { urlPattern: '*/whep*', requestStage: 'Response' }] });
  }
  info() { return { hostIp: this.hostIp, rewrites: this.rewrites, fetchError: this.fetchError || null, workerError: this.error || null, queueDiscipline: QUEUE_DISCIPLINE, config: this.cfg }; }
}
