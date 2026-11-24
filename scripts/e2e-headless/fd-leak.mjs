// Join/leave descriptor-leak test: one `serve` host, N real-Chrome join/leave cycles against the
// same process, then M cycles whose browser is killed, with an lsof census of the host between.
//   node scripts/e2e-headless/fd-leak.mjs --host local|<user@ip> --binary <path> --out <file.json>
//        [--mode leak|sizes|idle|churn] [--cycles 25] [--abrupt 5] [--done-file <remote DONE file>]
//   leak   descriptor census over join/leave cycles (default)
//   sizes  one host, one join per --sizes entry with motion-window on it: size, origin, overlaps
//   idle   one host, --cycles joins with >= 60 s of nothing between them: attach failures
//   churn  one host, --cycles joins 1 s apart
// Holds /tmp/sebench/encoder.lock while anything streams and gives it up for the long idle waits.
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const LAUNCHER = path.join(here, 'mac-host.sh');
const MOVER_LOCAL = path.resolve(here, '../../src-tauri/target/e2e/cursor-mover');
const argv = process.argv.slice(2);
const opt = (k, d) => { const i = argv.indexOf('--' + k); return i >= 0 ? argv[i + 1] : d; };
const HOST = opt('host', 'local'), LOCAL = HOST === 'local';
const BIN = opt('binary'), OUT = path.resolve(opt('out', 'fd-leak.json'));
const CYCLES = +opt('cycles', 25), ABRUPT = +opt('abrupt', 5), GATE = opt('done-file', null);
const HTTP = '18080', HTTPS = '18443';
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const run = (cmd, args, { env, input, timeoutMs = 60000 } = {}) => new Promise((res) => {
  const c = spawn(cmd, args, { env: { ...process.env, ...env } });
  let out = '', err = ''; c.stdout.on('data', (d) => { out += d; }); c.stderr.on('data', (d) => { err += d; });
  const t = setTimeout(() => c.kill('SIGKILL'), timeoutMs);
  c.on('close', (code) => { clearTimeout(t); res({ code, out, err }); });
  c.stdin.on('error', () => {}); c.stdin.end(input || '');
});
const ssh = (cmd) => run('ssh', ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', HOST, cmd]);
const onHost = (args, env = {}) => (LOCAL ? run('/bin/bash', [LAUNCHER, ...args], { env })
  : run('ssh', ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', HOST, 'bash', '-s', '--', ...args], { input: Object.entries(env).map(([k, v]) => `export ${k}='${v}'\n`).join('') + fs.readFileSync(LAUNCHER, 'utf8') }));
const kv = (t) => Object.fromEntries(t.split('---LSOF---')[0].split('\n').filter((l) => /^[A-Z_]+=/.test(l)).map((l) => [l.slice(0, l.indexOf('=')), l.slice(l.indexOf('=') + 1)]));

const MODE = opt('mode', 'leak');
// A local host creates a virtual display on this Mac and reflows whatever its user has open, so it
// has to be allowed explicitly.
if (LOCAL && process.env.E2E_ALLOW_LOCAL_DISPLAY !== '1') { console.error('refusing --host local: it creates a virtual display on this Mac. Set E2E_ALLOW_LOCAL_DISPLAY=1 only when its user has agreed.'); process.exit(65); }
const SIZES = (opt('sizes', '1280x720,2560x1440,1920x1080,800x600,3840x2160,1920x1080')).split(',');
const MOTION_LOCAL = path.resolve(here, '../../src-tauri/target/e2e/motion-window');
if (!LOCAL) for (;;) {
  const q = await ssh(`grep -q '^DONE' '${GATE}' && ! pgrep -f 'ScreenExtend-bench[^ ]* (bench|serve)' >/dev/null && echo FREE`);
  if (/FREE/.test(q.out)) break;
  await sleep(30000);
}
// The lock is a child process holding it until its stdin closes.
let lockChild = null;
async function lock() {
  if (lockChild) return;
  const c = spawn('lockf', ['-k', '/tmp/sebench/encoder.lock', '/bin/sh', '-c', 'echo HELD; cat >/dev/null'], { stdio: ['pipe', 'pipe', 'ignore'] });
  await new Promise((res, rej) => { c.stdout.on('data', (d) => { if (/HELD/.test(String(d))) res(); }); c.on('exit', () => rej(new Error('lockf exited'))); });
  lockChild = c;
}
async function unlock() { if (!lockChild) return; const c = lockChild; lockChild = null; await new Promise((res) => { c.on('exit', res); c.stdin.end(); }); }

const SID = Array.from(crypto.randomBytes(12), (b) => 'ABCDEFGHJKLMNPQRSTUVWXYZ23456789'[b % 32]).join(''), OTP = String(crypto.randomInt(0, 1e6)).padStart(6, '0');
const lan = Object.entries(os.networkInterfaces()).filter(([n]) => /^en\d/.test(n)).flatMap(([, a]) => a).find((a) => a.family === 'IPv4' && !a.internal).address;
const IP = LOCAL ? lan : HOST.split('@').pop();
const R = { schema: 'screenextend-fd-leak/1', started_at: new Date().toISOString(), host: HOST, binary: BIN, cycles: CYCLES, abrupt: ABRUPT, checkpoints: [], joins: [], failures: [] };
let dir = null, pid = null, chrome = null, profile = null, port = 0, baseDisplays = null;
const mover = LOCAL ? MOVER_LOCAL : (await ssh('echo $HOME')).out.trim() + '/se-e2e/cursor-mover';
const log = async () => (LOCAL ? fs.readFileSync(path.join(dir, 'serve.log'), 'utf8') : (await ssh(`cat '${dir}/serve.log'`)).out);
const displayCount = async () => { const p = kv((await onHost(['probe', mover])).out); try { return JSON.parse(p.DISPLAYS).length; } catch { return null; } };
const count = (t, re) => (t.match(re) || []).length;

async function census(label) {
  const r = await onHost(['fds', String(pid)]);
  const k = kv(r.out), lines = (r.out.split('---LSOF---\n')[1] || '').split('\n').slice(1).filter(Boolean);
  const by = {}, entries = [];
  let numbered = 0;
  for (const l of lines) {
    const f = l.trim().split(/\s+/); // COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME...
    const fd = f[3], type = f[4], rest = f.slice(5).join(' ');
    const isFd = /^\d+/.test(fd); if (isFd) numbered++;
    const st = (rest.match(/\(([A-Z_0-9]+)\)\s*$/) || [])[1];
    const proto = /\bTCP\b/.test(rest) ? 'TCP' : /\bUDP\b/.test(rest) ? 'UDP' : '';
    const key = isFd ? [type, proto, st].filter(Boolean).join(' ') : `mapped/${fd}`;
    by[key] = (by[key] || 0) + 1;
    if (isFd) entries.push({ fd, type, rest });
  }
  const c = { label, t_s: +((Date.now() - T0) / 1000).toFixed(1), lsof_lines: lines.length, descriptors: numbered, threads: +k.THREADS, rss_mb: +(k.RSS_KB / 1024).toFixed(1), mach_ports: +k.MACH_PORTS || null, displays: await displayCount(), by, entries };
  R.checkpoints.push(c);
  console.log(`[${c.t_s}s] ${label}: descriptors ${c.descriptors} lsof ${c.lsof_lines} threads ${c.threads} rss ${c.rss_mb} MB displays ${c.displays}`, JSON.stringify(by));
  if (!numbered) throw new Error('lsof census of the host is empty; host gone?');
}
function freePort() { return new Promise((res) => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => res(p)); }); }); }
async function launchChrome() {
  port = await freePort();
  chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, '--ignore-certificate-errors', '--autoplay-policy=no-user-gesture-required', '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--mute-audio', '--window-size=1280,720', '--no-first-run', '--no-default-browser-check', '--disable-background-networking', '--disable-component-update', '--disable-sync', 'about:blank'], { stdio: 'ignore' });
  chrome.gone = false; chrome.on('exit', function () { this.gone = true; });
  for (let i = 0; i < 100; i++) { try { await (await fetch(`http://127.0.0.1:${port}/json/version`)).json(); return; } catch { await sleep(200); } }
  throw new Error('Chrome never answered');
}
async function cdp(url) {
  const ws = new WebSocket(url); await new Promise((a, b) => { ws.onopen = a; ws.onerror = b; });
  let id = 0; const pend = new Map();
  ws.onmessage = (m) => { const d = JSON.parse(m.data);
    // A live page asks before it is left (beforeunload); an unanswered prompt blocks the leave.
    if (d.method === 'Page.javascriptDialogOpening') ws.send(JSON.stringify({ id: ++id, method: 'Page.handleJavaScriptDialog', params: { accept: true } })); const p = pend.get(d.id); if (p) { pend.delete(d.id); p(d); } };
  return { send: (method, params = {}) => new Promise((res) => { const n = ++id; pend.set(n, res); ws.send(JSON.stringify({ id: n, method, params })); setTimeout(() => { if (pend.delete(n)) res({ error: { message: 'timeout ' + method } }); }, 10000); }), close: () => ws.close() };
}
async function join(n, w = 1280, h = 720) {
  const t = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: 'PUT' })).json();
  const p = await cdp(t.webSocketDebuggerUrl);
  const ev = async (e) => { const r = await p.send('Runtime.evaluate', { expression: e, returnByValue: true, awaitPromise: true, userGesture: true }); return r.result && r.result.result ? r.result.result.value : null; };
  await p.send('Page.enable');
  await p.send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1, mobile: false, screenWidth: w, screenHeight: h });
  const nav = await p.send('Page.navigate', { url: `https://${IP}:${HTTPS}/?id=${SID}&name=FdLeak&otp=${OTP}&fs=0` });
  if (nav.result && nav.result.errorText) throw new Error('navigate: ' + nav.result.errorText);
  let submittedAt = 0, painted = 0, last = null, t0 = Date.now();
  for (let i = 0; i < 200 && !painted; i++) {
    const s = await ev(`(() => { const shown = (id) => { const m = document.getElementById(id); if (!m) return false; const c = getComputedStyle(m); return c.display !== 'none' && c.visibility !== 'hidden' && (m.classList.contains('show') || m.style.display === 'flex'); };
      for (const [a, b] of [['sameDeviceModal','sameDeviceDismiss'],['compatModal','compatContinue'],['permModal','permContinue']]) if (shown(a)) document.getElementById(b).click();
      const c = document.getElementById('connect'); return { ready: document.readyState === 'complete' && !!document.getElementById('joinForm') && c && !c.disabled, form: !!(document.getElementById('joinForm') || {}).offsetParent, painted: window.SEVideo ? SEVideo.getStats().framesPainted : 0, err: (document.getElementById('errorMessage') || {}).textContent || '' }; })()`);
    last = s;
    // Again after 4 s if the form is still up: a submit while a modal (same-device) was open is swallowed.
    if (s && s.ready && s.form && i > 6 && Date.now() - submittedAt > 4000) { await ev('document.getElementById("joinForm").requestSubmit()'); submittedAt = Date.now(); }
    painted = (s && s.painted) || 0;
    if (!painted) await sleep(150);
  }
  if (!painted) { const e = new Error(`cycle ${n}: nothing painted; page ${JSON.stringify(last)}`); e.p = p; throw e; };
  const ms = Date.now() - t0; await sleep(3000);
  return { p, ev, ms };
}
async function waitEnded(before, limitS) {
  const t0 = Date.now();
  while (Date.now() - t0 < limitS * 1000) {
    if (count(await log(), /ended \([^)]*\); removing display/g) > before && (await displayCount()) === baseDisplays) return Date.now() - t0;
    await sleep(500);
  }
  return null;
}
const leave = async (j) => { await j.p.send('Page.navigate', { url: 'about:blank' }); await sleep(300); await j.p.send('Page.close'); j.p.close(); };
async function otherModes() {
  const N = MODE === 'sizes' ? SIZES.length : CYCLES;
  for (let i = 1; i <= N; i++) {
    if (MODE === 'idle') { await unlock(); await sleep(60000); await lock(); }
    if (kv((await onHost(['probe', mover])).out).LOCKED === '1') throw new Error(`host screen locked before join ${i}; stopping`);
    const L0 = await log(), before = count(L0, /ended \([^)]*\); removing display/g), tl0 = count(L0, /\[display\] attach timeline/g), af0 = count(L0, /did not attach within timeout/g);
    const [w, h] = MODE === 'sizes' ? SIZES[i - 1].split('x').map(Number) : [1280, 720];
    const rec = { i, requested: `${w}x${h}`, t_s: +((Date.now() - T0) / 1000).toFixed(1) };
    R.joins.push(rec);
    let j;
    try { j = await join(i, w, h); rec.first_paint_ms = j.ms; } catch (e) { rec.error = e.message; R.failures.push(e.message); if (e.p) await leave({ p: e.p }); }
    if (j && MODE === 'sizes') {
      const d = JSON.parse(kv((await onHost(['probe', mover])).out).DISPLAYS).filter((x) => x.active);
      const v = d.find((x) => !x.builtin && x.vendor === 13398);
      rec.cg = v && [v.x, v.y, v.w, v.h]; rec.actual = v && `${v.w}x${v.h}`;
      rec.overlap = !!v && d.some((o) => o.id !== v.id && v.x < o.x + o.w && o.x < v.x + v.w && v.y < o.y + o.h && o.y < v.y + v.h);
      const st = async () => { const q = JSON.parse((await onHost(['status', BIN, dir])).out); const ss = Object.values(q.telemetry.sessions); return ss.sort((a, b) => b.frames_encoded - a.frames_encoded)[0]; };
      const painted = () => j.ev('SEVideo.getStats().framesPainted');
      const out = `/tmp/se-motion-${crypto.randomBytes(4).toString('hex')}.json`;
      const win = LOCAL ? onHost(['motionwin', MOTION_LOCAL, String(v && v.id), 'drag', '17', out, 'direct']) : null;
      await sleep(1500);
      const s0 = await st(), p0 = await painted(), t0 = Date.now(); await sleep(14000); const s1 = await st(), p1 = await painted(), dt = (Date.now() - t0) / 1000;
      rec.fresh_per_s = +((s1.stages.capture_to_encode.count - s0.stages.capture_to_encode.count) / dt).toFixed(1);
      rec.painted_fps = +((p1 - p0) / dt).toFixed(1);
      if (win) { try { rec.presented_fps = JSON.parse(kv((await win).out).MOTION).presented_fps; } catch { rec.presented_fps = null; } }
      if (rec.actual !== rec.requested) R.failures.push(`join ${i}: display ${rec.actual} != ${rec.requested}`);
      if (rec.overlap) R.failures.push(`join ${i}: ${rec.requested} overlaps another display`);
      if (rec.presented_fps && rec.fresh_per_s < 0.8 * rec.presented_fps) R.failures.push(`join ${i}: ${rec.requested} fresh ${rec.fresh_per_s}/s of ${rec.presented_fps} presented`);
    }
    if (j) { await leave(j); rec.ended_ms = await waitEnded(before, 30); if (rec.ended_ms == null) R.failures.push(`join ${i}: not ended in 30 s`); }
    const L1 = await log();
    rec.attach = L1.split('\n').filter((l) => /\[display\] attach timeline/.test(l)).slice(tl0);
    rec.attach_failures = count(L1, /did not attach within timeout/g) - af0;
    if (rec.attach_failures) R.failures.push(`join ${i}: ${rec.attach_failures} attach failure(s)`);
    console.log(JSON.stringify(rec).slice(0, 600));
    if (MODE === 'churn') await sleep(1000);
  }
}
let T0 = Date.now(), done = false;
async function cleanup() {
  if (done) return; done = true;
  if (chrome && !chrome.gone) chrome.kill('SIGKILL');
  if (pid || dir) {
    if (MODE === 'sizes') { const w = Number(kv((await onHost(['killwin'])).out).KILLED_WINDOWS); if (w) R.failures.push(`${w} motion window(s) still up; killed`); }
    const s = kv((await onHost(['stop', BIN, dir, HTTP, HTTPS])).out); R.stop = s;
    if (s.STOPPED_BY !== 'stop') R.failures.push(`host did not stop within 20 s; ${s.STOPPED_BY}`);
    if (s.LEFT_PID || (s.LEFT_LISTENERS || '').trim() || (s.LEFT_PROCS || '').trim()) R.failures.push('host left something behind');
    try { fs.writeFileSync(OUT.replace(/\.json$/, '.serve.log'), (await log()).split(OTP).join('######')); } catch {}
    if (LOCAL) fs.rmSync(dir, { recursive: true, force: true }); else await ssh(`rm -rf '${dir}'`);
    R.displays_after_stop = await displayCount();
  }
  await unlock();
  if (profile) { await sleep(500); try { fs.rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 300 }); } catch {} }
}
for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => { R.failures.push(sig); cleanup().then(() => { fs.writeFileSync(OUT, JSON.stringify(R, null, 1)); process.exit(1); }); });
try {
  await lock();
  R.mode = MODE; R.locked_start = kv((await onHost(['probe', mover])).out).LOCKED;
  if (R.locked_start === '1') throw new Error('host screen is locked; stopping');
  baseDisplays = await displayCount(); R.displays_before = baseDisplays;
  dir = LOCAL ? fs.mkdtempSync(path.join(os.tmpdir(), 'se-e2e-fd-')) : (await ssh('mktemp -d /tmp/se-e2e-fd.XXXXXX')).out.trim();
  const st = kv((await onHost(['start', BIN, dir, HTTP, HTTPS], { SCREENEXTEND_SESSION_ID: SID, SCREENEXTEND_OTP: OTP })).out);
  if (st.READY !== '1') throw new Error('host did not start: ' + st.ERROR);
  pid = +st.PID; T0 = Date.now(); R.pid = pid;
  profile = fs.mkdtempSync(path.join(os.tmpdir(), 'se-e2e-fdchrome-'));
  await sleep(3000); await census('before first join');
  await launchChrome();
  if (MODE !== 'leak') await otherModes();
  for (let i = 1; MODE === 'leak' && i <= CYCLES; i++) {
    const before = count(await log(), /ended \([^)]*\); removing display/g);
    const j = await join(i);
    await j.p.send('Page.navigate', { url: 'about:blank' }); await sleep(300); // pagehide -> /leave
    await j.p.send('Page.close'); j.p.close();
    const ended = await waitEnded(before, 30);
    R.joins.push({ i, kind: 'clean', first_paint_ms: j.ms, ended_ms: ended });
    if (ended == null && i === 1) throw new Error('cycle 1 never ended: harness or host cannot leave; not running 24 more');
    if (ended == null) R.failures.push(`cycle ${i}: session not ended / display not removed in 30 s`);
    if (i % 5 === 0) await census(`after ${i} clean cycles`);
  }
  if (MODE === 'leak') { await unlock(); await sleep(60000); await census('60 s after the last clean leave'); await lock(); }
  for (let i = 1; MODE === 'leak' && i <= ABRUPT; i++) {
    if (!chrome || chrome.gone) await launchChrome();
    const before = count(await log(), /ended \([^)]*\); removing display/g);
    const j = await join(CYCLES + i);
    chrome.kill('SIGKILL'); for (let k = 0; k < 50 && !chrome.gone; k++) await sleep(100);
    j.p.close();
    const ended = await waitEnded(before, 60);
    R.joins.push({ i: CYCLES + i, kind: 'abrupt', first_paint_ms: j.ms, ended_ms: ended });
    if (ended == null) R.failures.push(`abrupt ${i}: session not ended / display not removed in 60 s`);
  }
  if (MODE === 'leak') { await census(`after ${ABRUPT} abrupt kills`); await unlock(); await sleep(90000); await census('90 s after the last abrupt kill'); }
  const keyOf = (e) => `${e.type} ${e.rest.replace(/0x[0-9a-f]+/g, '0x…')}`;
  const tally = (c) => { const o = {}; for (const e of c.entries) { const k = keyOf(e); o[k] = (o[k] || 0) + 1; } return o; };
  const a = tally(R.checkpoints[0]), b = tally(R.checkpoints[R.checkpoints.length - 1]);
  R.diff_first_last = { added: {}, removed: {} };
  for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) { const d = (b[k] || 0) - (a[k] || 0); if (d > 0) R.diff_first_last.added[k] = d; if (d < 0) R.diff_first_last.removed[k] = -d; }
} catch (e) { R.failures.push(e.message); }
try {
  R.locked_end = kv((await onHost(['probe', mover])).out).LOCKED;
  const L = await log();
  R.attach_timeline = L.split('\n').filter((l) => /\[display\] attach timeline/.test(l));
  R.display_lines = L.split('\n').filter((l) => /^\[display\]|did not attach|came up at|remote-input display|refused/.test(l) && !/attach timeline/.test(l)).slice(0, 400);
  R.attach_failures = count(L, /did not attach within timeout/g);
} catch { /* host never started */ }
await cleanup();
R.finished_at = new Date().toISOString();
fs.writeFileSync(OUT, JSON.stringify(R, null, 1));
console.log('failures', R.failures, 'diff', JSON.stringify(R.diff_first_last, null, 1), OUT);
process.exit(R.failures.length ? 2 : 0);
