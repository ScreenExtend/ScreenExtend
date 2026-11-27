// Offline check of lib/relay.mjs: no host, no browser, no display. Two loopback sockets stand in
// for the host and the browser, and the down direction is measured from the browser's side.
//   node scripts/test-handler/relay-selftest.mjs
import dgram from 'node:dgram';
import { Relay } from './lib/relay.mjs';

const now = () => Number(process.hrtime.bigint()) / 1e6;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const median = (a) => { const s = a.slice().sort((x, y) => x - y); return s.length ? s[s.length >> 1] : null; };
let failed = 0;
function check(name, ok, detail) { if (!ok) failed++; console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}: ${detail}`); }

async function rig(cfg = {}) {
  const host = dgram.createSocket('udp4'), browser = dgram.createSocket('udp4');
  await new Promise((r) => host.bind(0, '127.0.0.1', r));
  await new Promise((r) => browser.bind(0, '127.0.0.1', r));
  const relay = new Relay({ hostIp: '127.0.0.1', cfg });
  const port = await relay.portFor(host.address().port, '127.0.0.1');
  // the browser speaks first, which is how the relay learns the way back
  const peer = await new Promise((res) => { host.once('message', (m, r) => res(r)); browser.send(Buffer.from('hello'), port, '127.0.0.1'); });
  const got = [];
  browser.on('message', (m) => got.push({ seq: m.readUInt32BE(0), sent: m.readDoubleBE(4), at: now(), len: m.length }));
  let seq = 0; const sent = [];
  const send = (len) => { const b = Buffer.alloc(len); const t = now(); b.writeUInt32BE(seq, 0); b.writeDoubleBE(t, 4); sent.push(t); seq++; host.send(b, peer.port, peer.address); };
  // owed-packet pacing: exact on average whatever the timer resolution
  const pace = (kbps, len, ms) => new Promise((res) => {
    const start = now(); let n = 0;
    const iv = setInterval(() => {
      const el = Math.min(now() - start, ms); const owed = Math.floor(el * kbps / 8 / len);
      while (n < owed) { send(len); n++; }
      if (el >= ms) { clearInterval(iv); res(n); }
    }, 1);
  });
  return { relay, got, sent, pace, close: async () => { await relay.stop(); host.close(); browser.close(); } };
}

async function delayStep() {
  const r = await rig({ down: { delay_ms: 5 } });
  await r.relay.arm([{ at_s: 0.8, down: { delay_ms: 60 } }, { at_s: 1.6, down: { delay_ms: 5 } }]);
  const t0 = now();
  await r.pace(320, 200, 2400); await sleep(300);
  const win = (a, b) => median(r.got.filter((g) => g.sent - t0 >= a * 1000 && g.sent - t0 < b * 1000).map((g) => g.at - g.sent));
  const d1 = win(0.2, 0.7), d2 = win(1.1, 1.5), d3 = win(2.0, 2.4);
  let ordered = true; for (let i = 1; i < r.got.length; i++) if (r.got[i].seq <= r.got[i - 1].seq) ordered = false;
  check('delay step 5 -> 60 -> 5 ms', d1 >= 4 && d1 <= 9 && d2 >= 58 && d2 <= 66 && d3 >= 4 && d3 <= 9, `median one-way ${d1.toFixed(1)} / ${d2.toFixed(1)} / ${d3.toFixed(1)} ms`);
  check('delay step keeps order and loses nothing', ordered && r.got.length === r.sent.length, `${r.got.length} of ${r.sent.length} arrived, in order: ${ordered}`);
  await r.close();
}

async function lossStep() {
  const r = await rig({ down: {} });
  await r.relay.arm([{ at_s: 0.5, down: { loss: 0.03 } }, { at_s: 2.5, down: {} }]);
  const t0 = now();
  await r.pace(800, 100, 3000); await sleep(200);
  const arrived = new Set(r.got.map((g) => g.seq));
  const lost = (a, b) => { let n = 0, all = 0; r.sent.forEach((t, i) => { if (t - t0 >= a * 1000 && t - t0 < b * 1000) { all++; if (!arrived.has(i)) n++; } }); return { n, all }; };
  const p1 = lost(0, 0.45), p2 = lost(0.55, 2.45), p3 = lost(2.55, 3.0);
  const share = p2.n / p2.all;
  const st = await r.relay.stats(0);
  check('loss step 0 -> 3 % -> 0', p1.n === 0 && p3.n === 0 && share >= 0.018 && share <= 0.045, `lost ${p1.n}/${p1.all}, ${p2.n}/${p2.all} (${(share * 100).toFixed(2)} %), ${p3.n}/${p3.all}`);
  check('loss step drops are counted as loss', st.totals.down.lossDrops === r.sent.length - r.got.length && st.totals.down.queueDrops === 0, `lossDrops ${st.totals.down.lossDrops}, missing ${r.sent.length - r.got.length}`);
  await r.close();
}

async function queueDepth(queueMs) {
  const r = await rig({ down: {} });
  const cfg = queueMs ? { rate_kbps: 1000, queue_ms: queueMs } : { rate_kbps: 1000 };
  const v = await r.relay.set({ down: cfg });
  await r.pace(2000, 1000, 2000); await sleep(800);
  const st = await r.relay.stats(0);
  const maxDelay = Math.max(...r.got.map((g) => g.at - g.sent));
  const want = queueMs || 100;
  const qb = Math.round(1000 * 1000 / 8 * want / 1000);
  check(`queue of ${want} ms at 1 Mbit/s`, v.down.queue_bytes === qb && maxDelay >= 0.85 * want && maxDelay <= 1.15 * want + 20 && st.totals.down.queueDrops > 0 && st.totals.down.maxQueueBytes <= qb,
    `queue_bytes ${v.down.queue_bytes} (want ${qb}), max one-way ${maxDelay.toFixed(0)} ms, queue drops ${st.totals.down.queueDrops}, max queue ${st.totals.down.maxQueueBytes} B`);
  await r.close();
}

async function baselineCap() {
  const r = await rig({ down: {} });
  await r.relay.markBaseline();
  await r.pace(2000, 1000, 1500);
  const v = await r.relay.set({ down: { rate_of_baseline: 0.98, queue_ms: 500 } });
  const b = v.down.baseline_kbps, rate = v.down.rate_kbps;
  check('baseline-relative cap measures the input rate', b >= 1860 && b <= 2040 && Math.abs(rate - 0.98 * b) <= 1 && v.down.queue_bytes === Math.round(rate * 1000 / 8 * 0.5),
    `baseline ${b} kbit/s, cap ${rate} kbit/s, queue_bytes ${v.down.queue_bytes}`);
  const g0 = r.got.length; const tFill = now();
  await r.pace(2000, 1000, 5000);
  const st = await r.relay.stats(0);
  const tail = r.got.slice(g0).filter((g) => g.at - tFill >= 2000);
  const outKbps = tail.length > 1 ? (tail.reduce((x, g) => x + g.len, 0) - tail[0].len) * 8 / (tail[tail.length - 1].at - tail[0].at) : 0;
  const rows = st.seconds.slice(-4).map((x) => x.down.queueBytes);
  check('a cap just under the input rate fills its queue slowly', st.totals.down.queueDrops === 0 && rows[rows.length - 1] > 2000 && rows[rows.length - 1] < 60000 && Math.abs(outKbps - rate) / rate < 0.05,
    `out ${outKbps.toFixed(0)} kbit/s against cap ${rate}, queue by second ${rows.join(' ')} B, drops ${st.totals.down.queueDrops}`);
  // a later phase reuses the baseline even though the input has changed since
  await r.pace(4000, 1000, 500);
  await r.relay.arm([{ at_s: 0.05, down: { rate_of_baseline: 0.5 } }]); await sleep(300);
  const c = (await r.relay.stats(0)).config.down;
  check('a later phase reuses the measured baseline', c.baseline_kbps === b && Math.abs(c.rate_kbps - 0.5 * b) <= 1, `baseline ${c.baseline_kbps}, cap ${c.rate_kbps} kbit/s`);
  await r.close();
}

async function noMark() {
  const r = await rig({ down: {} });
  const v = await r.relay.set({ down: { rate_of_baseline: 0.98 } });
  check('a baseline-relative cap with no baseline is refused, not guessed', !v.down.rate_kbps && v.down.rate_error === 'no baseline marked', `rate_kbps ${v.down.rate_kbps}, error ${v.down.rate_error}`);
  await r.close();
}

await delayStep();
await lossStep();
await queueDepth(0);
await queueDepth(500);
await baselineCap();
await noMark();
console.log(failed ? `${failed} check(s) failed` : 'all relay checks passed');
process.exit(failed ? 1 : 0);
