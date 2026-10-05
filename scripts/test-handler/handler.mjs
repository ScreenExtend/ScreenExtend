#!/usr/bin/env node
// ScreenExtend test handler. See README.md (and TEST-WORKFLOW.md §5) for the contract.
//
//   node scripts/test-handler/handler.mjs --exe <ScreenExtend.exe> [--request <file.json | inline JSON>]
//
// Prints ONE JSON document on stdout. Exit 0 = the harness ran to completion (whatever the
// numbers say); non-zero = the harness itself failed, with the reason in the JSON.
//
// Rules this file keeps (TEST-WORKFLOW.md §3): no input is injected by any route; processes are
// killed only by the PIDs this run started; the owner's config.json is never left changed.
import { spawn, execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ROOT, acquireSlot, acquireAll, releaseSlots, measureLock, pidAlive, slotStatus } from './lib/slots.mjs';
import { probeBuild, startHost, stopHost, runCli, cliJson, randomSessionId, randomOtp } from './lib/host.mjs';
import { Client, stampMetrics } from './lib/client.mjs';
import { driverState, driverRemove } from './lib/driver.mjs';
import { startLoadSampler, snapshot, waitForQuiet, memSnapshot } from './lib/load.mjs';
import { dist, spread } from './lib/metrics.mjs';
import { sleep } from './lib/cdp.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const VERSION = 1;

// ---------------------------------------------------------------- request
const DEFAULTS = {
  scenarios: ['static', 'small_window', 'large_area', 'hires_small_window'],
  duration: 20,            // seconds per timed window
  static_duration: 30,
  warmup: 3,               // seconds of motion before a timed window opens
  runs: 1,
  client: { width: 1920, height: 1080, dpr: 1, refresh: 60, https: true },
  hires_client: { width: 1280, height: 720, dpr: 2, refresh: 120, https: true },
  host_env: {}, host_args: [], chrome_flags: [], headed: false,
  calibrate: true, force_exclusive: false, screencast_width: 480, overlap_displays: false,
  audio: false, mic_prompt: false, settings_change: false, two_clients: false, leave_rejoin: false,
  slow_answer: false, auto_rejoin: false, logs: false, fallback_paths: false, loss: false, custom: null,
  mover_stop: false,       // true or {cycles, move_s, hold_s, step_ms, speed}: where does the picture come to rest?
  netlog: false,           // Chrome's own network log per browser, into the run directory
  stamp_band: 'full',      // 'full': average each stamp cell's whole height; 'center': a 2-row band (immune to stage tearing)
  // After every join has left: close the host's stdout/stderr pipes (as a parent that stops
  // reading does), make it log, and report whether its control server still answers.
  pipe_close: false,
  // Before every timed window: wait up to max_wait_s for whole-machine CPU (2 s average) to fall
  // below max_cpu. A window that opened loaded, or whose own mean CPU ended above max_cpu, is
  // marked `loaded: true` and should be discarded and repeated. `load_gate: false` turns it off.
  load_gate: { max_cpu: 75, max_wait_s: 180 },
};
const KNOWN = new Set([...Object.keys(DEFAULTS), 'label', 'notes']);

/** Anything that would reach the host's input injector is refused, with the reason. */
function inputRefusal(req) {
  const keys = Object.keys(req).filter((k) => /input|mouse|keyboard|pointer|touch|cursor|key_?press|clipboard|wheel/i.test(k));
  if (keys.length) return `request keys ${JSON.stringify(keys)} ask for input; input injection is forbidden (TEST-WORKFLOW.md §3 rule 1: the owner's cursor and keyboard are off limits)`;
  const js = req.custom && typeof req.custom.js === 'string' ? req.custom.js : '';
  const banned = /(new\s+(Mouse|Pointer|Keyboard|Wheel|Touch|Input|Focus|Drag|Clipboard)Event)|dispatchEvent|Input\.dispatch|\.focus\(|requestPointerLock|keyboard\.lock|initMouseEvent|createEvent|RemoteInput|sendInput/i;
  if (js && banned.test(js)) return 'custom.js creates or dispatches events (or touches RemoteInput); the client page forwards such events to the host, which would move the real cursor. Refused.';
  const cmds = (req.custom && req.custom.host_commands) || [];
  for (const c of cmds) { const s = [].concat(c).join(' '); if (/measure-input|measure-stream|test-startup-gate|--control\b|pointer-scope/i.test(s)) return `custom.host_commands entry "${s}" touches input; refused.`; }
  if (req.chrome_flags && [].concat(req.chrome_flags).some((f) => /remote-debugging-port|user-data-dir/.test(f))) return 'chrome_flags may not override the slot\'s debugging port or profile';
  return null;
}

function parseArgs(argv) {
  const a = { exe: null, request: {} };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--exe') a.exe = argv[++i];
    else if (argv[i] === '--request') { const v = argv[++i]; a.request = v.trim().startsWith('{') ? JSON.parse(v) : JSON.parse(fs.readFileSync(v, 'utf8')); }
    else if (argv[i] === '--status') a.status = true;
    else if (argv[i] === '--calibrate-only') a.calibrateOnly = true;
    else if (argv[i] === '--help' || argv[i] === '-h') a.help = true;
  }
  return a;
}

// ---------------------------------------------------------------- shared state for one run
const R = { handler: VERSION, startedAt: new Date().toISOString(), ok: false, warnings: [], errors: [], notRun: [], measurementWindows: [] };
const LOGS = [];
let RUN_DIR = null;
const log = (s) => { const line = `${new Date().toISOString().slice(11, 23)} ${s}`; LOGS.push(line); process.stderr.write(line + '\n'); };
const owned = { hosts: [], clients: [], stages: [] }; // everything to tear down, by PID

const REAL_CONFIG = path.join(process.env.APPDATA || '', 'app.screenextend.desktop', 'config.json');
const sha = (f) => { try { return crypto.createHash('sha256').update(fs.readFileSync(f)).digest('hex'); } catch { return null; } };

// Every timed window goes through here, so the lock is held for exactly that window.
let lockHeld = false;
async function underLock(label, fn) {
  if (lockHeld) return fn(); // already inside a window of this run
  lockHeld = true;
  try { return await underLockOnce(label, fn); } finally { lockHeld = false; }
}
async function underLockOnce(label, fn) {
  const lk = await measureLock(label, log);
  const w = { label, lockWaitMs: lk.waitedMs, start: lk.acquiredAt, end: null };
  try { return await fn(); } finally {
    w.end = lk.release(); R.measurementWindows.push(w);
    try { fs.appendFileSync(path.join(ROOT, 'measure-log.jsonl'), JSON.stringify({ pid: process.pid, slot: R.slot, label, start: w.start, end: w.end }) + '\n'); } catch { /* best effort */ }
  }
}

// ---------------------------------------------------------------- stage (on-screen content)
// Which \\.\DISPLAYn carries the display this join made. The bundled driver gives every
// monitor the EDID name "VirtuDisplay+", so names cannot be used: ask the host (current builds
// report each join's display device), else take the one screen that appeared since host start.
async function resolveGdi(name, { quiet = false } = {}) {
  const j = await hostJoins();
  if (quiet && !(j.joins || []).some((x) => x.name === name)) return { error: 'no join yet' };
  if (j.joins) {
    const mine = j.joins.filter((x) => x.name === name && x.display && x.display.device);
    if (mine.length === 1) return { gdi: mine[0].display.device, driverId: mine[0].display.id, via: 'host status --json (driver id -> device path)' };
    // Two live joins for one browser is the retried-offer defect; the newest is the one the
    // page is now showing.
    if (mine.length > 1) return { gdi: mine[mine.length - 1].display.device, driverId: mine[mine.length - 1].display.id, via: `host status --json (newest of ${mine.length} joins named ${name})`, duplicateJoins: mine.length };
  }
  const now = screens();
  const added = now.filter((s) => !(HOSTCTX.screensBefore || []).includes(s));
  if (added.length === 1) return { gdi: added[0], via: 'the one screen attached since the host started' };
  return { error: `cannot tell which display is ours (new screens: ${JSON.stringify(added)})` };
}

function screens() {
  try {
    return execFileSync('powershell', ['-NoProfile', '-Command', 'Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Screen]::AllScreens | % { $_.DeviceName }'], { encoding: 'utf8', windowsHide: true }).split(/\r?\n/).map((x) => x.trim()).filter(Boolean);
  } catch { return []; }
}

function startStage(gdi, { seconds, stamp = true, mover = false, large = false, notIn = null, wait = 20, driverId = null, extra = [] }) {
  const args = ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(HERE, 'stage.ps1'), '-Seconds', String(seconds), '-WaitSeconds', String(wait)];
  if (driverId != null) args.push('-DriverId', String(driverId));
  if (gdi) args.push('-Gdi', gdi); else args.push('-NewScreenNotIn', (notIn || []).join(';'));
  if (stamp) args.push('-Stamp'); if (mover) args.push('-Mover'); if (large) args.push('-Large');
  args.push(...extra);
  const proc = spawn('powershell', args, { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
  owned.stages.push(proc);
  const st = { current: null };
  let out = ''; let ready = null; let done = null; let resolveReady;
  const readyP = new Promise((r) => { resolveReady = r; });
  proc.stdout.on('data', (d) => {
    out += d;
    for (const line of out.split('\n')) { try { const j = JSON.parse(line.trim()); if (j.ready || j.error) { st.current = j; if (!ready) { ready = j; resolveReady(j); } } if (j.done) done = j; } catch { /* partial */ } }
  });
  proc.stderr.on('data', (d) => { out += d; });
  const exited = new Promise((r) => proc.on('exit', (code) => { if (!ready) resolveReady({ error: 'stage exited before ready: ' + out.slice(-500), code }); r(code); }));
  return Object.assign(st, { proc, ready: Promise.race([readyP, sleep(45000).then(() => ({ error: 'stage not ready in 45 s' }))]), exited, done: () => done, output: () => out });
}

/** Start the stage on the display of the join named `name`. */
async function stageFor(name, opts) {
  let r = await resolveGdi(name);
  for (let i = 0; i < 10 && r.error; i++) { await sleep(500); r = await resolveGdi(name); }
  if (r.error) return { proc: { exitCode: 0, kill() {} }, ready: Promise.resolve({ error: r.error }), exited: Promise.resolve(0), done: () => null, output: () => '' };
  const st = startStage(r.gdi, { ...opts, driverId: r.driverId }); st.gdiVia = r.via; return st;
}

function stopStage(st) { if (st && st.proc.exitCode === null) { try { st.proc.kill(); } catch { /* gone */ } } }

// ---------------------------------------------------------------- host helpers
let H = null;          // the current host handle
let HOSTCTX = null;    // { exe, dataDir, env, ports, sessionId, otp, build, mode }

async function bootHost(label) {
  const c = HOSTCTX;
  // Display-topology changes (a create, a removal, a startup reclaim) stall every display's
  // compositor for a moment, so anything that can make one runs under the measurement lock too.
  const h = await underLock('host-start', () => startHost({ exe: c.exe, dataDir: c.dataDir, httpPort: c.ports.http, httpsPort: c.ports.https, sessionId: c.sessionId, otp: c.otp, envCredentials: c.build.envCredentials, extraArgs: c.hostArgs, extraEnv: c.hostEnv, logFile: path.join(RUN_DIR, 'serve.log') }));
  owned.hosts.push(h);
  log(`host ${label}: pid ${h.pid} up=${h.up} ip=${h.ip} in ${h.startupMs} ms`);
  return h;
}

const cliEnv = () => (HOSTCTX.dataDir ? { SCREENEXTEND_DATA_DIR: HOSTCTX.dataDir } : {});
const cli = (args, timeout) => runCli(HOSTCTX.exe, args, { env: cliEnv(), timeout });

async function hostJoins() {
  const r = await cli(['status', '--json']);
  const j = cliJson(r);
  if (!j) return { available: false, raw: (r.stdout + r.stderr).slice(0, 300) };
  const t = j.telemetry;
  return { available: !!(t && t.joins), joins: t && t.joins ? t.joins : null, telemetry: t || null, status: j };
}

/** Our host's monitors in the driver. Isolated: our pid's id block; exclusive: everything new. */
async function ourMonitors() {
  const st = await driverState();
  if (!Array.isArray(st)) return { error: st && st.error };
  if (HOSTCTX.mode === 'isolated') {
    // Shared-driver mode records each monitor's owner pid (0 = released, still attached).
    let owners = {};
    try { owners = JSON.parse(fs.readFileSync(path.join(os.tmpdir(), 'screenextend-shared-driver', 'owners.json'), 'utf8')); } catch { /* none yet */ }
    const mine = st.filter((m) => H && owners[m.id] === H.pid);
    return { monitors: mine.map((m) => ({ id: m.id, name: m.name })), driverTotal: st.length, releasedAttached: st.filter((m) => owners[m.id] === 0).map((m) => m.id) };
  }
  const base = new Set((HOSTCTX.driverBefore || []).map((m) => m.id));
  return { monitors: st.filter((m) => !base.has(m.id)).map((m) => ({ id: m.id, name: m.name })), driverTotal: st.length };
}

function serveLogCounts() {
  let s = ''; try { s = fs.readFileSync(path.join(RUN_DIR, 'serve.log'), 'utf8'); } catch { /* none */ }
  return { connected: (s.match(/\+ device connected/g) || []).length, disconnected: (s.match(/- device disconnected/g) || []).length, bytes: s.length };
}

async function deviceKey(name) {
  // Address the device by token where the build reports one (devices set refuses an address
  // shared by two devices); the old build only knows addresses.
  const j = await hostJoins();
  if (j.joins) { const mine = j.joins.filter((x) => x.name === name); if (mine.length) return { key: mine[mine.length - 1].token, by: 'token', joins: j.joins }; }
  return { key: H.ip, by: 'ip (this build reports no join tokens)', joins: null };
}

// ---------------------------------------------------------------- client helpers
function clientUrl(c, name, { https = true } = {}) {
  const origin = https ? `https://${H.ip}:${HOSTCTX.ports.https}` : `http://${H.ip}:${HOSTCTX.ports.http}`;
  return { origin, url: `${origin}/?id=${HOSTCTX.sessionId}&name=${encodeURIComponent(name)}&otp=${HOSTCTX.otp}&fs=0` };
}

const STD_GRANT = ['audioCapture', 'wakeLockScreen', 'speakerSelection'];
let CLIENT_SEQ = 0, PROFILE_SEQ = 0;
const deviceName = () => `SEH${R.slot}${crypto.randomBytes(2).toString('hex').toUpperCase()}${++CLIENT_SEQ}`.slice(0, 13);

async function openClient(spec, { flags = [], disable = [], offset = 0 } = {}) {
  const port = 19200 + R.slot * 10 + offset;
  // A fresh directory per browser: a profile Chrome has just let go of can stay locked for a while.
  const profile = path.join(ROOT, `profile-slot${R.slot}-${offset}-${process.pid}-${++PROFILE_SEQ}`);
  const netlog = REQ.netlog && RUN_DIR ? [`--log-net-log=${path.join(RUN_DIR, `netlog-${offset}-${PROFILE_SEQ}.json`)}`] : [];
  const cl = new Client({ port, profile, flags: [...REQ.chrome_flags, ...flags, ...netlog], headed: REQ.headed, log, scWidth: REQ.screencast_width });
  cl.stampBand = REQ.stamp_band; owned.clients.push(cl);
  await cl.open({ client: spec, observer: { refreshRate: spec.refresh, disable } });
  return cl;
}

async function joinClient(cl, spec, { name = deviceName(), grant = STD_GRANT, micChoice = 'continue' } = {}) {
  const { origin, url } = clientUrl(spec, name, { https: spec.https !== false });
  // A stage waiting before the submit draws its first stamp the moment the join's display
  // attaches; the first frame showing it is the first painted frame. A rejoin that keeps its
  // display draws on that one.
  const pre = await resolveGdi(name, { quiet: true });
  const j = await underLock('join', async () => {
    const known = screens();
    const factory = () => (pre.gdi ? startStage(pre.gdi, { seconds: 45, wait: 30, driverId: pre.driverId }) : startStage('', { seconds: 45, notIn: known, wait: 30 }));
    const restage = async (old) => {
      stopStage(old); const r = await resolveGdi(name, { quiet: true });
      if (r.error) {
        const n = screens().filter((x) => !known.includes(x));
        if (n.length) return startStage(n[n.length - 1], { seconds: 45, wait: 10 });
        // A build that reports no joins (bef5f89) and reused the display it kept through the
        // disconnect grace: no screen appeared with this join, so the "one new screen" the first
        // stage waited for never comes. The one screen added since the host started is it.
        const added = screens().filter((x) => !(HOSTCTX.screensBefore || []).includes(x));
        return added.length === 1 ? startStage(added[0], { seconds: 45, wait: 10 }) : null;
      }
      return startStage(r.gdi, { seconds: 45, wait: 10, driverId: r.driverId });
    };
    try { return await cl.join({ url, origin, grant, micChoice, stage: factory, restage }); } finally { stopStage(cl.joinStage); cl.joinStage = null; }
  });
  j.name = name;
  log(`join ${name}: answer ${j.answerMs} ms, first picture ${j.firstPictureMs} ms${j.error ? ' ERROR ' + j.error : ''}${j.pageError ? ' page: ' + j.pageError : ''}`);
  return j;
}

async function leave(cl) {
  // Under the lock: the leave removes a display (after the host's settle delay).
  await underLock('leave', async () => {
    const before = await ourMonitors();
    await cl.page.send('Page.navigate', { url: 'about:blank' }).catch(() => {});
    const n0 = before.monitors ? before.monitors.length : 0;
    for (let i = 0; i < 16; i++) { await sleep(500); const m = await ourMonitors(); if (!m.monitors || m.monitors.length < n0 || n0 === 0) break; }
    await sleep(500);
  });
}

// ---------------------------------------------------------------- clock + calibration
async function clockOffset(cl) {
  // browser Date.now() minus system clock (node's Date.now), from the minimum-RTT sample.
  let best = null;
  for (let i = 0; i < 25; i++) {
    const t0 = performance.now();
    const b = await cl.page.ev('performance.timeOrigin + performance.now()');
    const t1 = performance.now();
    const s = { rtt: t1 - t0, off: b - (performance.timeOrigin + (t0 + t1) / 2) };
    if (!best || s.rtt < best.rtt) best = s;
  }
  return { browserMinusSystemMs: Math.round(best.off * 100) / 100, rttMs: Math.round(best.rtt * 100) / 100, method: 'min-RTT of 25 Runtime.evaluate round trips; node performance.timeOrigin+now() is the system clock reference' };
}

async function calibrate(cl, spec) {
  // The method's own floor: a local page (no stream) draws the same stamp from its own Date.now()
  // into a canvas, wraps that canvas in a VideoFrame and paints it with drawImage at 60 Hz —
  // exactly what the client page does with a decoded frame — and the paint hook reads it back.
  // What is left is readback error and stamp quantisation; nothing of the host or the network.
  await cl.page.send('Page.navigate', { url: 'about:blank' }); await sleep(300);
  await cl.page.ev(`(() => {
    document.body.style.margin = 0; document.body.style.background = '#000';
    const src = new OffscreenCanvas(${spec.width}, ${spec.height}); const g = src.getContext('2d');
    const cv = document.createElement('canvas'); cv.width = ${spec.width}; cv.height = ${spec.height};
    cv.style.cssText = 'display:block;width:100vw;height:100vh'; document.body.appendChild(cv);
    const out = cv.getContext('2d'); const cell = Math.max(8, Math.min(64, Math.floor(${spec.width} / 40)));
    g.fillStyle = '#336'; g.fillRect(0, 0, ${spec.width}, ${spec.height});
    setInterval(() => {
      const ms = Date.now(); const v = ms & 0xFFFFFF; const chk = ((((v >> 16) & 255) + ((v >> 8) & 255) + (v & 255)) & 255) ^ 0x5A;
      const bits = [1, 0, 1, 0]; for (let i = 23; i >= 0; i--) bits.push((v >> i) & 1); for (let i = 7; i >= 0; i--) bits.push((chk >> i) & 1); bits.push(0, 1);
      bits.forEach((b, i) => { g.fillStyle = b ? '#fff' : '#000'; g.fillRect(i * cell, 0, cell, cell); });
      const f = new VideoFrame(src, { timestamp: ms * 1000 }); out.drawImage(f, 0, 0); f.close();
    }, 16); })()`);
  await sleep(500);
  const cell = Math.max(8, Math.min(64, Math.floor(spec.width / 40)));
  const stage = { width: spec.width, height: spec.height, cell, cellH: cell };
  return underLock('calibration', async () => {
    const { samples, counters } = await cl.collectStamps(stage, 6000);
    const m = stampMetrics(samples, { from: -Infinity, to: Infinity, offsetMs: 0 });
    m.windowSeconds = 6; m.paintedFps = Math.round((m.newStamps / 6) * 10) / 10; m.hookCounters = counters;
    await cl.page.send('Page.navigate', { url: 'about:blank' }).catch(() => {});
    return { ...m, note: 'local page, no stream: a stamp from the page\'s own Date.now() painted as a VideoFrame through drawImage every 16 ms and read back by the same paint hook. This is the floor of the method (readback error + 1 ms stamp quantisation); it excludes the client compositor, exactly as every stream measurement does.' };
  });
}

// ---------------------------------------------------------------- load gate
async function loadGate(label) {
  const g = REQ.load_gate;
  if (!g) return null;
  const r = await waitForQuiet({ maxCpu: g.max_cpu ?? 75, maxWaitMs: (g.max_wait_s ?? 180) * 1000 });
  log(`load gate (${label}): ${r.loaded ? 'still loaded' : 'quiet'} after ${r.waitedMs} ms, cpu ${r.cpuAtOpen} %`);
  return r;
}
// A window is "loaded" if the gate gave up, or if the window's own mean CPU ended above the bar.
function loadedVerdict(gate, loadSum) {
  const bar = REQ.load_gate ? (REQ.load_gate.max_cpu ?? 75) : null;
  const mean = loadSum && loadSum.cpuPct ? loadSum.cpuPct.mean : null;
  return { loaded: !!((gate && gate.loaded) || (bar != null && mean != null && mean > bar)), loadGate: gate, windowCpuMean: mean };
}

// ---------------------------------------------------------------- one timed window
async function timedWindow(cl, name, { seconds, mover, large, label }) {
  // The gate runs before the stage starts: the stage process lives for a fixed time, and a
  // gate that waited inside the window would outlast it.
  const gate = await loadGate(label);
  // The noise is painted by its own process, so a slow paint can never hold the stamp back.
  const st = await stageFor(name, { seconds: seconds + REQ.warmup + 4, stamp: true, mover, large: false });
  const ready = await st.ready;
  if (ready.error) { stopStage(st); return { error: 'stage: ' + ready.error }; }
  const lg = large ? startStage(ready.gdi, { seconds: seconds + REQ.warmup + 4, stamp: false, large: true }) : null;
  if (lg) { const lr = await lg.ready; if (lr.error) { stopStage(st); stopStage(lg); return { error: 'large-area stage: ' + lr.error }; } }
  return underLock(label, async () => {
    await sleep(REQ.warmup * 1000);
    const load = startLoadSampler();
    const secBefore = await cl.secondary();
    const from = Date.now() + 200; // browser clock is within a ms of this (see clock)
    const got = await cl.collectStamps(ready, seconds * 1000 + 400, { follow: st });
    const to = from + seconds * 1000;
    const secAfter = await cl.secondary();
    const loadSum = load.stop();
    await st.exited.catch(() => {}); stopStage(st); if (lg) { await lg.exited.catch(() => {}); stopStage(lg); }
    const m = stampMetrics(got.samples, { from: from + R.clock.browserMinusSystemMs, to: to + R.clock.browserMinusSystemMs, offsetMs: R.clock.browserMinusSystemMs });
    return { ...m, ...loadedVerdict(gate, loadSum), stage: ready, stageDone: st.done(), layout: got.layout, load: loadSum, secondaryBefore: secBefore, secondaryAfter: secAfter, pageCounters: pageDelta(secBefore, secAfter, seconds) };
  });
}

function pageDelta(a, b, seconds) {
  const out = {};
  const va = a && a.seVideo, vb = b && b.seVideo;
  if (va && vb && typeof vb === 'object') {
    out.seVideo = { videoPath: vb.videoPath, audioPath: vb.audioPath, paintedFps: vb.framesPainted != null && va.framesPainted != null ? Math.round(((vb.framesPainted - va.framesPainted) / seconds) * 10) / 10 : null, framesDropped: (vb.framesDropped || 0) - (va.framesDropped || 0), decodeErrors: vb.decodeErrors, keyRequests: vb.keyRequests, hostToPaint: vb.hostToPaint, decode: vb.decode, arrivalToPaint: vb.arrivalToPaint, rttMs: vb.rttMs, decoderHardware: vb.decoderHardware, transportTier: vb.transportTier, renderTier: vb.renderTier };
  }
  const wa = a && a.webrtc && a.webrtc.video, wb = b && b.webrtc && b.webrtc.video;
  if (wa && wb) out.webrtc = { decodedFps: Math.round(((wb.framesDecoded - wa.framesDecoded) / seconds) * 10) / 10, lost: wb.lost - wa.lost, pli: wb.pli - wa.pli, nack: wb.nack - wa.nack, keyFrames: wb.keyFrames - wa.keyFrames, decoder: wb.decoder, codec: wb.codec, frame: `${wb.frameWidth}x${wb.frameHeight}` };
  if (b && b.seAudio) out.seAudio = b.seAudio;
  return out;
}

// ---------------------------------------------------------------- standard scenarios
async function standardScenarios(cl, spec) {
  const out = {};
  const want = (s) => REQ.scenarios.includes(s);
  const common = ['static', 'small_window', 'large_area'].filter(want);
  if (common.length || REQ.audio || REQ.settings_change || REQ.logs || REQ.custom) {
    const j = await joinClient(cl, spec);
    R.joins.push({ scenario: 'common', ...j });
    if (j.firstPictureMs == null) { out.common = { error: 'no picture after join', join: j }; }
    else {
      if (want('static')) {
        await sleep(3000); // let the join's own stage window disappear and the stream settle
        out.static = await underLock('static', async () => {
          const gate = await loadGate('static');
          const load = startLoadSampler(); const a = await cl.secondary();
          const s = await cl.collectStatic(REQ.static_duration * 1000);
          const b = await cl.secondary(); const loadSum = load.stop();
          return { ...s, ...loadedVerdict(gate, loadSum), seconds: REQ.static_duration, client: spec, load: loadSum, pageCounters: pageDelta(a, b, REQ.static_duration), note: 'no stage window: the virtual display shows only the desktop. Every changed frame is listed; a healthy stream has none after it settles.' };
        });
      }
      for (const [sc, mover, large] of [['small_window', true, false], ['large_area', false, true]]) {
        if (!want(sc)) continue;
        const runs = [];
        for (let r = 0; r < REQ.runs; r++) runs.push(await timedWindow(cl, j.name, { seconds: REQ.duration, mover, large, label: `${sc}#${r + 1}` }));
        out[sc] = { client: spec, runs, aggregate: aggregate(runs) };
      }
    }
    if (REQ.audio) out.audio = await audioTest(cl, j.name);
    if (REQ.settings_change) R.special.settings_change = await settingsChange(cl, j.name);
    if (REQ.logs) R.special.logs = await logsTest();
    if (REQ.custom) R.special.custom = await customHook(cl);
    await leave(cl);
  }
  if (want('hires_small_window') || want('hires_large_area')) {
    const hs = { ...REQ.hires_client };
    await cl.setClient(hs);
    const j = await joinClient(cl, hs);
    R.joins.push({ scenario: 'hires', ...j });
    if (j.firstPictureMs == null) out.hires = { error: 'no picture after join', join: j };
    else for (const [sc, mover, large] of [['hires_small_window', true, false], ['hires_large_area', false, true]]) {
      if (!want(sc)) continue;
      const runs = [];
      for (let r = 0; r < REQ.runs; r++) runs.push(await timedWindow(cl, j.name, { seconds: REQ.duration, mover, large, label: `${sc}#${r + 1}` }));
      out[sc] = { client: hs, runs, aggregate: aggregate(runs) };
    }
    await leave(cl);
    await cl.setClient(spec);
  }
  return out;
}

function aggregate(runs) {
  const ok = runs.filter((r) => !r.error);
  const f = (get) => spread(ok.map(get));
  return {
    runs: runs.length, completed: ok.length,
    latencyP50: f((r) => r.latencyMs.p50), latencyP90: f((r) => r.latencyMs.p90), latencyP99: f((r) => r.latencyMs.p99), latencyMax: f((r) => r.latencyMs.max),
    paintedFps: f((r) => r.paintedFps), longestGapMs: f((r) => r.longestGapMs),
    loadedRuns: ok.filter((r) => r.loaded).length,
  };
}

// ---------------------------------------------------------------- audio
async function audioTest(cl, name) {
  const res = { };
  const k = await deviceKey(name);
  const set = await cli(['devices', 'set', k.key, '--audio', 'on']);
  res.enable = { by: k.by, code: set.code, out: (set.stdout + set.stderr).trim().slice(0, 300) };
  if (HOSTCTX.mode === 'exclusive') res.enable.note = 'wrote the real config.json; it is restored byte-for-byte at the end of the run';
  await sleep(4000); // the first enable renegotiates once (TEST-WORKFLOW §5.6)
  if (/restart it or reconnect/i.test(res.enable.out)) {
    // Older builds apply a saved override only to a new connection — and `bef5f89`'s `serve`
    // reads device overrides from config.json only at startup, so there the host is restarted
    // (same credentials and ports) before the rejoin.
    await leave(cl);
    if (!R.host.controlVerbs) {
      res.hostRestart = await stopHost(H, { exe: HOSTCTX.exe, env: cliEnv(), useStopVerb: false });
      H = await bootHost('restart-for-audio');
    }
    res.rejoin = await joinClient(cl, REQ.client, { name });
    res.enable.note = (res.enable.note ? res.enable.note + '; ' : '') + (R.host.controlVerbs ? 'this build applies the override on reconnect, so the device left and rejoined' : 'this build reads overrides only at serve startup, so the host was restarted and the device rejoined');
    await sleep(2000);
  }
  return underLock('audio', async () => {
    await cl.page.ev('__se.startOnsets(0.02, 30)');
    await sleep(2500);
    const before = await cl.page.ev('__se.audioBuckets.slice()');
    const clicks = await new Promise((resolve) => {
      const p = spawn('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(HERE, 'clicks.ps1'), '-Seconds', '12'], { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
      owned.stages.push(p); let o = ''; p.stdout.on('data', (d) => { o += d; }); p.stderr.on('data', (d) => { o += d; });
      p.on('exit', () => { try { resolve(JSON.parse(o.trim().split('\n').pop())); } catch { resolve({ error: o.slice(-400) }); } });
    });
    await sleep(2500);
    const page = await cl.page.ev('({ onsets: __se.onsets.slice(), buckets: __se.audioBuckets.slice(), analysers: __se.analysers.length, ctx: __se.analysers.map((a) => ({ state: a.ctx.state, sr: a.ctx.sampleRate, outputLatencyMs: a.ctx.outputLatency * 1000, baseLatencyMs: a.ctx.baseLatency * 1000 })) })');
    await cl.page.ev('__se.stopOnsets()');
    const sec = await cl.secondary();
    res.clicksPlayed = clicks.clicks ? clicks.clicks.length : 0; res.player = { ...clicks, clicks: undefined };
    res.analysers = page.analysers; res.audioContexts = page.ctx;
    const off = R.clock.browserMinusSystemMs;
    const toRender = [], toOutput = [], peaks = []; const matched = new Set();
    for (const c of clicks.clicks || []) {
      const hit = page.onsets.findIndex((o, i) => !matched.has(i) && o.render - off >= c - 20 && o.render - off <= c + 650);
      if (hit < 0) continue; matched.add(hit);
      const o = page.onsets[hit]; toRender.push(o.render - off - c); if (o.output) toOutput.push(o.output - off - c); peaks.push(o.peak);
    }
    res.arrived = matched.size > 0;
    res.matched = matched.size; res.unmatchedOnsets = page.onsets.length - matched.size;
    res.delayToRenderMs = dist(toRender);
    res.delayToOutputMs = dist(toOutput);
    res.clickPeak = dist(peaks);
    const firstClick = clicks.clicks && clicks.clicks[0];
    const quiet = (arr) => arr.filter((b) => b.rms != null).map((b) => b.rms);
    res.silenceBefore = dist(quiet(before));
    res.silenceAfter = dist(quiet(page.buckets.filter((b) => firstClick && b.t - off > clicks.clicks[clicks.clicks.length - 1] + 800)));
    res.path = sec.seAudio ? sec.seAudio.path : null;
    res.transport = sec.seAudio ? sec.seAudio.transport : null;
    res.syncInfo = sec.seAudio || null;
    res.activePath = sec.audioActivePath;
    res.dataChannels = sec.webrtc ? sec.webrtc.dataChannels : null; res.audioTrack = sec.webrtc ? sec.webrtc.audioTrack : null;
    res.fallbackHints = cl.console.filter((c) => /\[audio\]|fallback|AudioDecoder|worklet/i.test(c.text)).slice(-15);
    res.definitions = { host: 'click time = when winmm reported the click\'s first sample consumed (clicks.ps1)', render: 'onset found by an analyser spliced before the destination, timed by AudioContext.currentTime', output: 'render time mapped through getOutputTimestamp (adds the browser\'s output latency)', note: 'Chrome runs with --mute-audio so the host does not capture the client\'s playback; the graph still renders. Other sounds on the machine are captured too and show up as unmatched onsets.' };
    return res;
  });
}

// ---------------------------------------------------------------- settings change
async function settingsChange(cl, name) {
  const out = { edits: [] };
  const k0 = await deviceKey(name);
  const edits = [
    ['audio on', ['--audio', 'on']], ['audio off', ['--audio', 'off']],
    ['video quality', ['--video-quality', '30']], ['video scale', ['--video-scale', '75']],
    ['refresh rate', ['--refresh-rate', '120']], ['DPR (native-dpr off)', ['--native-dpr', 'off']],
    ['orientation portrait', ['--orientation', 'Portrait']], ['orientation landscape', ['--orientation', 'Landscape']],
    ['display scale', ['--scale', '150']], ['name', null],
  ];
  for (const [label, args] of edits) {
    const before = { joins: (await hostJoins()).joins, monitors: await ourMonitors(), log: serveLogCounts(), pcs: await cl.page.ev('__se.pcs.length').catch(() => null), whep: await cl.page.ev('__se.whep.length').catch(() => null) };
    const st = await stageFor(name, { seconds: 16, stamp: true });
    const ready = await st.ready;
    if (ready.error) { out.edits.push({ label, error: 'stage: ' + ready.error }); stopStage(st); continue; }
    const res = await underLock('settings:' + label, async () => {
      const collect = cl.collectStamps(ready, 13000, { follow: st });
      await sleep(2500);
      const editAt = Date.now(); let cmd;
      if (args) cmd = await cli(['devices', 'set', k0.key, ...args]);
      else cmd = await renameDevice(k0, name);
      const got = await collect;
      const off = R.clock.browserMinusSystemMs;
      const after = got.samples.filter((s) => s.ts - off >= editAt && s.stamp !== undefined);
      let lastTs = editAt + off, maxGap = 0, gapEnd = null; let prevStamp = null;
      for (const s of after) { if (s.stamp === prevStamp) continue; prevStamp = s.stamp; const g = s.ts - lastTs; if (g > maxGap) { maxGap = g; gapEnd = s.ts; } lastTs = s.ts; }
      return { cmd: { code: cmd.code, out: (cmd.stdout + cmd.stderr).trim().slice(0, 240) }, largestGapAfterEditMs: Math.round(maxGap), pictureRecoveredMs: maxGap > 150 && gapEnd ? Math.round(gapEnd - off - editAt) : 0, framesAfterEdit: after.length, metrics: stampMetrics(got.samples, { from: editAt + off, to: editAt + off + 10000, offsetMs: off }) };
    });
    stopStage(st);
    await sleep(1500);
    const after = { joins: (await hostJoins()).joins, monitors: await ourMonitors(), log: serveLogCounts(), pcs: await cl.page.ev('__se.pcs.length').catch(() => null), whep: await cl.page.ev('__se.whep.length').catch(() => null) };
    const mine = (j) => (j || []).filter((x) => x.name === name || (x.token && x.token === k0.key));
    const jb = mine(before.joins), ja = mine(after.joins);
    out.edits.push({
      label, args,
      liveJoinsBefore: before.joins ? before.joins.length : null, liveJoinsAfter: after.joins ? after.joins.length : null,
      joinIdBefore: jb.map((x) => x.joinId), joinIdAfter: ja.map((x) => x.joinId), joinIdChanged: jb.length && ja.length ? jb[0].joinId !== ja[0].joinId : null,
      displaysBefore: before.monitors.monitors ? before.monitors.monitors.length : null, displaysAfter: after.monitors.monitors ? after.monitors.monitors.length : null,
      displayIdBefore: jb.map((x) => x.display && x.display.id), displayIdAfter: ja.map((x) => x.display && x.display.id),
      driverMonitorsAfter: after.monitors.monitors,
      deviceRemovedEvents: after.log.disconnected - before.log.disconnected, deviceJoinedEvents: after.log.connected - before.log.connected,
      newPeerConnections: after.pcs != null && before.pcs != null ? after.pcs - before.pcs : null, newWhepRequests: after.whep != null && before.whep != null ? after.whep - before.whep : null,
      ...res,
    });
    log(`settings ${label}: joins ${before.joins ? before.joins.length : '?'}->${after.joins ? after.joins.length : '?'}, displays ${before.monitors.monitors ? before.monitors.monitors.length : '?'}->${after.monitors.monitors ? after.monitors.monitors.length : '?'}, recovered ${res.pictureRecoveredMs} ms`);
  }
  out.defectSeen = out.edits.some((e) => (e.liveJoinsAfter || 0) > 1 || (e.displaysAfter || 0) > 1);
  out.note = 'Each edit runs alone with a fresh stamp window. "pictureRecoveredMs" is edit -> first new stamp after the largest gap (0 when no gap over 150 ms). The `name` edit goes through `config set devices.<i>.name` because `devices set` has no name flag.';
  return out;
}

async function renameDevice(k, name) {
  const g = await cli(['config', 'get', 'devices']);
  let rows = null; try { rows = JSON.parse(g.stdout.slice(g.stdout.indexOf('['))); } catch { /* no rows */ }
  const i = rows ? rows.findIndex((r) => r.token === k.key || r.ip === k.key || r.name === name) : -1;
  if (i < 0) return { code: 2, stdout: '', stderr: 'no saved device row to rename (the audio edit creates one on the current build)' };
  return cli(['config', 'set', `devices.${i}.name`, name + 'R']);
}

// ---------------------------------------------------------------- the other special requests
async function logsTest() {
  const l = await cli(['logs', '--lines', '50']);
  const s = await hostJoins();
  const out = { logs: { code: l.code, lines: l.stdout.split('\n').filter(Boolean).length, sample: l.stdout.split('\n').slice(-5) }, statusJson: { available: s.available, hasTelemetry: !!s.telemetry, joins: s.joins ? s.joins.length : null }, pass: l.code === 0 && l.stdout.trim().length > 0 && !!s.telemetry };
  // The whole backlog against the host's own `-v` echo in serve.log: is every line the bus kept
  // also one the host printed, did any multi-byte character arrive mangled (U+FFFD), and did the
  // bus report a drop.
  const full = await cli(['logs'], 30000);
  const busLines = full.stdout.split(/\r?\n/).filter((x) => x.length);
  const serveText = (() => { try { return fs.readFileSync(path.join(RUN_DIR, 'serve.log'), 'utf8'); } catch { return ''; } })();
  const echoed = new Set(serveText.split('\n').map((x) => x.replace(/^\[\d+\] /, '').replace(/\r$/, '')));
  const tailN = busLines.slice(-300);
  out.full = {
    code: full.code, lines: busLines.length, bytes: Buffer.byteLength(full.stdout),
    replacementChars: (full.stdout.match(/�/g) || []).length,
    replacementCharsInServeLog: (serveText.match(/�/g) || []).length,
    droppedReports: busLines.filter((x) => /\[logbus\] dropped/.test(x)),
    lastLinesAlsoEchoed: `${tailN.filter((x) => echoed.has(x)).length}/${tailN.length}`,
  };
  // `logs: {burst: N}` — N control connections with a wrong token, each of which the host logs
  // once ("presented a bad token"). That is a log burst made from outside the host with nothing
  // but its own loopback socket (no input, no display), enough to wrap the 2000-line ring.
  const burst = REQ.logs && typeof REQ.logs === 'object' && REQ.logs.burst;
  if (burst && HOSTCTX.dataDir) {
    const ctrl = fs.readFileSync(path.join(HOSTCTX.dataDir, 'screenextend.ctrl'), 'utf8').split(/\r?\n/);
    const port = Number(ctrl[0]);
    const net = await import('node:net');
    const t0 = Date.now();
    for (let i = 0; i < burst; i++) {
      await new Promise((resolve) => {
        const sock = net.connect(port, '127.0.0.1', () => sock.end(`not-the-token-${i} PING\n`));
        sock.on('close', resolve); sock.on('error', resolve); sock.resume();
      });
    }
    const sentMs = Date.now() - t0;
    await sleep(500);
    const after = await cli(['logs'], 30000);
    const al = after.stdout.split(/\r?\n/).filter((x) => x.length);
    out.burst = { sent: burst, sentMs, lines: al.length, badTokenLines: al.filter((x) => /presented a bad token/.test(x)).length, droppedReports: al.filter((x) => /\[logbus\] dropped/.test(x)), replacementChars: (after.stdout.match(/�/g) || []).length, lastLine: al[al.length - 1] };
  }
  return out;
}

// A `serve -v` whose reader goes away: a terminal that is closed under it, a `| Select -First n`,
// a supervising script that exits. Runs after every join has left, so no display is live.
async function pipeClose() {
  const out = { note: 'stdout/stderr pipes closed by the parent; the host was then made to log by bad-token control connections' };
  if (!HOSTCTX.dataDir) return { ...out, notRun: 'needs an isolated build (the control file is in the slot data directory)' };
  const before = await cli(['logs', '--lines', '3']);
  out.before = { logsCode: before.code };
  H.proc.stdout.destroy(); H.proc.stderr.destroy();
  await sleep(300);
  const ctrl = fs.readFileSync(path.join(HOSTCTX.dataDir, 'screenextend.ctrl'), 'utf8').split(/\r?\n/);
  const net = await import('node:net');
  for (let i = 0; i < 3; i++) {
    await new Promise((resolve) => {
      const sock = net.connect(Number(ctrl[0]), '127.0.0.1', () => sock.end(`not-the-token PING\n`));
      sock.on('close', resolve); sock.on('error', resolve); sock.resume();
    });
    await sleep(200);
  }
  const after = await cli(['logs', '--lines', '10'], 10000);
  const st = await cli(['status', '--json'], 10000);
  out.after = { logsCode: after.code, logsTail: after.stdout.split(/\r?\n/).filter(Boolean).slice(-6), logsErr: after.stderr.trim().slice(0, 300), statusCode: st.code, hostAlive: pidAlive(H.pid) };
  out.controlServerSurvived = after.code === 0;
  return out;
}

async function customHook(cl) {
  const c = REQ.custom; const out = {};
  if (c.js) { try { out.js = await cl.page.ev(c.js, { timeout: (c.timeout || 20) * 1000 }); } catch (e) { out.jsError = e.message; } }
  if (c.host_commands) { out.host = []; for (const cmd of c.host_commands) { const r = await cli([].concat(cmd)); out.host.push({ args: r.args, code: r.code, stdout: r.stdout.slice(0, 200000), stderr: r.stderr.slice(0, 1000) }); } }
  return out;
}

async function micPrompt() {
  const out = {};
  for (const [label, grant] of [['speaker access granted (no microphone)', ['speakerSelection', 'wakeLockScreen']], ['nothing granted', null]]) {
    const cl = await openClient(REQ.client, { offset: 3 });
    const j = await joinClient(cl, REQ.client, { grant, micChoice: 'skip' });
    const page = await cl.page.ev(`(async () => ({
      gum: __se.gum, permQueries: __se.permQueries,
      micPermission: await navigator.permissions.query({ name: 'microphone' }).then((p) => p.state, (e) => 'ERR ' + e.message),
      outputLabels: await navigator.mediaDevices.enumerateDevices().then((d) => d.filter((x) => x.kind === 'audiooutput').map((x) => x.label)),
      selectAudioOutput: !!(navigator.mediaDevices && navigator.mediaDevices.selectAudioOutput),
      setSinkId: 'setSinkId' in HTMLMediaElement.prototype }))()`).catch((e) => ({ error: e.message }));
    out[label] = { join: j, micCardShown: j.modals.some((m) => /^micCard/.test(m)), microphoneRequested: (page.gum || []).some((g) => /audio/.test(g.constraints)), ...page,
      outputsAvailableWithoutMic: (page.outputLabels || []).some((l) => l), defect: null };
    const o = out[label];
    if (o.outputsAvailableWithoutMic && (o.micCardShown || o.microphoneRequested)) o.defect = 'microphone prompt/card although output labels were already available';
    await leave(cl); await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1);
  }
  return out;
}

async function twoClients(clA) {
  const out = {};
  const ja = await joinClient(clA, REQ.client);
  const clB = await openClient(REQ.client, { offset: 1 });
  const jb = await joinClient(clB, REQ.client);
  out.joins = { a: ja, b: jb };
  if (jb.firstPictureMs == null || ja.firstPictureMs == null) { out.error = 'a client got no picture'; }
  else {
    const st = await stageFor(jb.name, { seconds: 30, stamp: true });
    const ready = await st.ready;
    if (ready.error) out.error = 'stage: ' + ready.error;
    else {
      const ka = await deviceKey(ja.name);
      out.result = await underLock('two_clients', async () => {
        const col = clB.collectStamps(ready, 24000, { follow: st });
        await sleep(6000); const baseEnd = Date.now();
        const edits = [];
        for (const args of [['--video-quality', '30'], ['--refresh-rate', '120'], ['--video-scale', '75']]) { const r = await cli(['devices', 'set', ka.key, ...args]); edits.push({ args, code: r.code, at: Date.now() }); await sleep(4500); }
        const got = await col; const off = R.clock.browserMinusSystemMs;
        const base = stampMetrics(got.samples, { from: baseEnd - 5500 + off, to: baseEnd + off, offsetMs: off });
        const during = stampMetrics(got.samples, { from: baseEnd + off, to: Date.now() + off, offsetMs: off });
        return { editsOnA: edits, bBaseline: base, bDuringEditsOnA: during, bUntouched: during.longestGapMs != null && during.longestGapMs < Math.max(150, 3 * (base.longestGapMs || 50)) };
      });
    }
    stopStage(st);
  }
  // The first client again, now that a second display exists beside its own: does its stream
  // still carry new frames?
  if (ja.firstPictureMs != null) {
    const sa = await stageFor(ja.name, { seconds: 14, stamp: true });
    const ra = await sa.ready;
    if (ra.error) out.aAfterBJoined = { error: 'stage: ' + ra.error };
    else out.aAfterBJoined = await underLock('two_clients:a', async () => { await sleep(1500); const t0 = Date.now(); const g = await clA.collectStamps(ra, 8000, { follow: sa }); const off = R.clock.browserMinusSystemMs; return { stage: ra, ...stampMetrics(g.samples, { from: t0 + off, to: Date.now() + off, offsetMs: off }) }; });
    stopStage(sa);
  }
  out.joinsAtEnd = (await hostJoins()).joins; out.monitorsAtEnd = await ourMonitors();
  await leave(clB); await clB.close(); owned.clients.splice(owned.clients.indexOf(clB), 1);
  await leave(clA);
  return out;
}

async function leaveRejoin(cl) {
  const out = {};
  const j1 = await joinClient(cl, REQ.client);
  const s1 = await hostJoins(); const m1 = await ourMonitors();
  const grace = s1.status && s1.status.disconnectGraceSecs ? s1.status.disconnectGraceSecs : null;
  out.first = { join: j1, joins: s1.joins, monitors: m1 };
  // Part 1: the tab goes away and a new one rejoins inside the grace period.
  await cl.page.send('Page.navigate', { url: 'about:blank' }).catch(() => {}); const leftAt = Date.now();
  await sleep(2000);
  const j2 = await joinClient(cl, REQ.client, { name: j1.name });
  const s2 = await hostJoins(); const m2 = await ourMonitors();
  out.rejoinWithinGrace = { rejoinedAfterMs: Date.now() - leftAt, join: j2, joins: s2.joins, monitors: m2, sameDisplayId: m1.monitors && m2.monitors && m1.monitors.length === 1 && m2.monitors.length === 1 ? m1.monitors[0].id === m2.monitors[0].id : null };
  // Part 2: leave and wait out the grace period; the display must go.
  await cl.page.send('Page.navigate', { url: 'about:blank' }).catch(() => {}); const left2 = Date.now();
  const timeline = [];
  for (let i = 0; i < 30; i++) { await sleep(1000); const m = await ourMonitors(); timeline.push({ s: Math.round((Date.now() - left2) / 1000), displays: m.monitors ? m.monitors.length : null }); if (m.monitors && m.monitors.length === 0 && i > 2) break; }
  out.afterLeave = { timeline, displayRemoved: timeline.length ? timeline[timeline.length - 1].displays === 0 : null, removedAfterS: (timeline.find((t) => t.displays === 0) || {}).s ?? null, configuredGraceSecs: grace };
  return out;
}

async function slowAnswer(cl) {
  const n = typeof REQ.slow_answer === 'object' && REQ.slow_answer.n ? REQ.slow_answer.n : 5;
  const out = { attempts: [] };
  const name = deviceName();
  for (let i = 0; i < n; i++) {
    const j = await joinClient(cl, REQ.client, { name });
    // Every attempt's full join record (network timing per /whep request included) goes into
    // joins[] too, so a slow or lost answer can be laid against serve.log afterwards.
    R.joins.push({ scenario: `slow_answer#${i + 1}`, ...j });
    const s = await hostJoins(); const m = await ourMonitors();
    const e = { i: i + 1, answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, pageError: j.pageError, liveJoins: s.joins ? s.joins.length : null, displays: m.monitors ? m.monitors.length : null };
    e.flag = (j.answerMs > 5000 ? 'answer over 5 s; ' : '') + (/did not answer|gave up|timed out/i.test(j.pageError || '') ? 'page gave up; ' : '') + ((e.liveJoins || 0) > 1 || (e.displays || 0) > 1 ? 'second session/display; ' : '');
    out.attempts.push(e);
    await leave(cl); await sleep(2500);
  }
  out.flagged = out.attempts.filter((a) => a.flag).length;
  return out;
}

// The cursor stand-in at rest. A small window moves (fast steps, like a pointer) and stops; the
// stage reports where it stopped. Does the client's picture come to rest there, or one step
// short? A frame gate that drops the frame carrying the final position leaves the picture stale
// until something else changes — on a still desktop, indefinitely.
async function moverStopTest(cl) {
  const o = typeof REQ.mover_stop === 'object' ? REQ.mover_stop : {};
  const cycles = o.cycles || 10, moveS = o.move_s || 1.2, holdS = o.hold_s || 2, stepMs = o.step_ms || 8, speed = o.speed || 2.5;
  const j = await joinClient(cl, REQ.client);
  R.joins.push({ scenario: 'mover_stop', ...j });
  if (j.firstPictureMs == null) { await leave(cl); return { error: 'no picture after join', join: { answerMs: j.answerMs, offers: j.offers } }; }
  await sleep(1500);
  const secs = Math.ceil(cycles * (moveS + holdS)) + 3;
  const st = await stageFor(j.name, { seconds: secs, stamp: false, mover: true, extra: ['-MoverStepMs', String(stepMs), '-MoverSpeed', String(speed), '-MoveSeconds', String(moveS), '-HoldSeconds', String(holdS)] });
  const ready = await st.ready;
  if (ready.error) { stopStage(st); await leave(cl); return { error: 'stage: ' + ready.error }; }
  const res = await underLock('mover_stop', async () => {
    await cl.drainStamps();
    await cl.setStampCfg({ mode: 'mover', width: ready.width, height: ready.height, y: Math.floor(ready.height * 0.4), h: 48 });
    const samples = []; const t0 = Date.now();
    while (st.proc.exitCode === null && Date.now() - t0 < (secs + 10) * 1000) { await sleep(250); samples.push(...(await cl.drainStamps()).samples); }
    await cl.setStampCfg(null); await sleep(100); samples.push(...(await cl.drainStamps()).samples);
    return samples.filter((x) => x.mx !== undefined);
  });
  stopStage(st);
  const off = R.clock.browserMinusSystemMs;
  const stopsRaw = st.output().split('\n').map((l) => { try { return JSON.parse(l.trim()); } catch { return null; } }).filter((x) => x && x.stop);
  const stops = stopsRaw.map((s) => {
    const inHold = res.filter((x) => x.t - off >= s.t && x.t - off <= s.t + holdS * 1000 - 50 && x.mx != null);
    const late = inHold.filter((x) => x.t - off >= s.t + holdS * 1000 * 0.6);
    const settled = late.length ? late[late.length - 1].mx : (inHold.length ? inHold[inHold.length - 1].mx : null);
    const reached = inHold.find((x) => Math.abs(x.mx - s.x) <= 6);
    return { stop: s.stop, trueX: s.x, settledX: settled == null ? null : Math.round(settled), errorPx: settled == null ? null : Math.round(settled - s.x), reachedMs: reached ? Math.round(reached.t - off - s.t) : null, paintsDuringHold: inHold.length, lastPaintMs: inHold.length ? Math.round(inHold[inHold.length - 1].t - off - s.t) : null };
  });
  const stale = stops.filter((s) => s.errorPx == null || Math.abs(s.errorPx) > 6);
  await leave(cl);
  const diag = { samples: res.length, found: res.filter((x) => x.mx != null).length, firstT: res.length ? Math.round(res[0].t - off) : null, lastT: res.length ? Math.round(res[res.length - 1].t - off) : null, firstStopT: stopsRaw.length ? stopsRaw[0].t : null, notFoundSamples: res.filter((x) => x.smp).slice(0, 5).map((x) => x.smp), stage: ready };
  return { diag, params: { cycles, moveS, holdS, stepMs, speedPxPerMs: speed, display: { width: ready.width, height: ready.height } }, stops, staleStops: stale.length, of: stops.length, note: 'settledX: where the orange window sits in the last painted frame of the hold (after 60 % of it), read from a 2-row band, 2 px resolution at 1920 wide; stale = more than 6 px from where the stage stopped it. reachedMs: stop -> first painted frame showing the true position.' };
}

async function autoRejoin(cl) {
  const out = {};
  const j = await joinClient(cl, REQ.client);
  out.join = j;
  const stop = await stopHost(H, { exe: HOSTCTX.exe, env: cliEnv(), useStopVerb: !!R.host.controlVerbs });
  out.stop = stop;
  const downAt = Date.now();
  await sleep(3000);
  H = await bootHost('restart');
  out.restartedAfterMs = Date.now() - downAt;
  const timeline = [];
  for (let i = 0; i < 30; i++) {
    await sleep(1000);
    const s = await hostJoins(); const g = await cl.geometry();
    timeline.push({ s: i + 1, hostJoins: s.joins ? s.joins.length : null, pageLive: g ? g.live : null, pageError: await cl.page.ev('(document.getElementById("errorMessage") || {}).textContent || ""').catch(() => null) });
    if (s.joins && s.joins.length) break;
  }
  out.timeline = timeline; out.rejoined = timeline.some((t) => t.hostJoins > 0);
  out.console = cl.console.filter((c) => c.t >= downAt).slice(0, 40);
  await leave(cl);
  return out;
}

async function fallbackPaths() {
  const tiers = [
    ['default', {}],
    ['no WebCodecs', { disable: ['webcodecs'] }],
    ['no RTCRtpScriptTransform', { disable: ['script_transform'] }],
    ['no script transform and no encoded streams', { disable: ['script_transform', 'encoded_streams'] }],
    ['plain HTTP (no secure context, no cross-origin isolation)', { https: false }],
    ['hardware decode off', { flags: ['--disable-accelerated-video-decode'] }],
  ];
  const out = {};
  for (const [label, o] of tiers) {
    const spec = { ...REQ.client, https: o.https !== false };
    const cl = await openClient(spec, { offset: 2, flags: o.flags || [], disable: o.disable || [] });
    try {
      const j = await joinClient(cl, spec);
      if (j.firstPictureMs == null) { out[label] = { error: 'no picture', join: j }; continue; }
      const w = await timedWindow(cl, j.name, { seconds: Math.min(REQ.duration, 10), mover: true, large: false, label: 'fallback:' + label });
      const sec = await cl.secondary();
      out[label] = { join: { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, secure: j.page && j.page.secure, coi: j.page && j.page.coi }, latencyMs: w.latencyMs, paintedFps: w.paintedFps, longestGapMs: w.longestGapMs, videoPath: sec.seVideo && sec.seVideo.videoPath, renderTier: sec.seVideo && sec.seVideo.renderTier, transportTier: sec.seVideo && sec.seVideo.transportTier, decoder: sec.webrtc && sec.webrtc.video ? sec.webrtc.video.decoder : null, usingFallback: sec.usingFallback, error: w.error };
      await leave(cl);
    } finally { await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1); }
  }
  return out;
}

// ---------------------------------------------------------------- main
let REQ = null;
let STATUS_ONLY = false;

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.status) { process.stdout.write(JSON.stringify(slotStatus(), null, 1) + '\n'); STATUS_ONLY = true; return 0; }
  if (args.calibrateOnly) {
    // The method's floor alone: no host, one slot, one browser.
    REQ = { ...DEFAULTS, ...args.request, client: { ...DEFAULTS.client, ...(args.request.client || {}) } };
    const s = await acquireSlot({ label: 'calibrate-only' }, log); R.slot = s.slot;
    try {
      const cl = await openClient(REQ.client);
      R.chrome = { version: cl.chrome.version };
      R.clock = await clockOffset(cl);
      R.calibration = await calibrate(cl, REQ.client);
      R.ok = true;
    } finally { R.cleanup = { clients: [] }; for (const c of owned.clients) R.cleanup.clients.push(await c.close()); releaseSlots([s.slot]); }
    return 0;
  }
  if (args.help || !args.exe) { process.stdout.write(fs.readFileSync(path.join(HERE, 'README.md'), 'utf8').split('\n').slice(0, 40).join('\n') + '\n'); return args.exe ? 0 : 1; }
  const unknown = Object.keys(args.request).filter((k) => !KNOWN.has(k));
  const refusal = inputRefusal(args.request);
  if (refusal) { R.refused = refusal; R.errors.push(refusal); return 3; }
  if (unknown.length) R.warnings.push('unknown request keys ignored: ' + unknown.join(', '));
  REQ = { ...DEFAULTS, ...args.request, client: { ...DEFAULTS.client, ...(args.request.client || {}) }, hires_client: { ...DEFAULTS.hires_client, ...(args.request.hires_client || {}) } };
  R.request = REQ;
  const exe = path.resolve(args.exe);
  if (!fs.existsSync(exe)) { R.errors.push('exe not found: ' + exe); return 2; }
  const build = probeBuild(exe); R.build = build;
  for (const [dll, present] of Object.entries(build.dlls)) if (!present) R.warnings.push(`${dll} is not beside the exe; ${dll === 'libopus.dll' ? 'system audio' : 'the x264 fallback'} cannot load. Copy it from src-tauri/resources/ of that revision.`);
  const mode = build.isolationSwitch && !REQ.force_exclusive ? 'isolated' : 'exclusive';
  R.mode = mode;
  if (mode === 'exclusive') R.warnings.push(build.isolationSwitch ? 'exclusive mode forced by the request' : 'this build has no SCREENEXTEND_DATA_DIR switch: exclusive mode (all slots held, the real data directory, config.json backed up and restored)');

  // ---- slots
  const info = { exe, label: REQ.label || null };
  let slots;
  if (mode === 'isolated') { const s = await acquireSlot(info, log); slots = [s.slot]; R.queueWaitMs = s.waitedMs; }
  else { const s = await acquireAll(info, log); slots = s.slots; R.queueWaitMs = s.waitedMs; }
  R.slot = slots[0]; R.slotsHeld = slots;
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  RUN_DIR = path.join(ROOT, 'runs', `${stamp}-slot${R.slot}-${process.pid}`); fs.mkdirSync(RUN_DIR, { recursive: true });
  R.runDir = RUN_DIR;
  log(`slot ${R.slot} (${mode}), run dir ${RUN_DIR}`);

  const cfgBefore = sha(REAL_CONFIG); R.ownerConfig = { path: REAL_CONFIG, sha256Before: cfgBefore };
  if (cfgBefore) fs.copyFileSync(REAL_CONFIG, path.join(RUN_DIR, 'config.json.before'));
  R.joins = []; R.special = {};
  try {
    const dataDir = mode === 'isolated' ? path.join(ROOT, `data-slot${R.slot}`) : null;
    if (dataDir) { fs.rmSync(dataDir, { recursive: true, force: true }); fs.mkdirSync(dataDir, { recursive: true }); }
    const ports = { http: 18000 + R.slot * 10, https: 18001 + R.slot * 10 };
    HOSTCTX = { exe, dataDir, ports, mode, build, sessionId: randomSessionId(), otp: randomOtp(), hostArgs: REQ.host_args, hostEnv: REQ.host_env, driverBefore: [] };
    const ds = await driverState(); HOSTCTX.driverBefore = Array.isArray(ds) ? ds : [];
    R.driverBefore = ds;
    R.loadBefore = await snapshot();

    HOSTCTX.screensBefore = screens();
    H = await bootHost('start');
    R.host = { pid: H.pid, args: H.args, credentialsVia: H.credentialsVia, ip: H.ip, ports, dataDir, startupMs: H.startupMs, startup: H.startup, up: H.up };
    if (!H.up) {
      R.host.logTail = H.logTail;
      const lockedOut = /already running/i.test(H.logTail || '');
      throw new Error(`host did not come up on ${H.ip || '?'}:${ports.http}${lockedOut ? ' (it reports another instance holds the lock: no isolation for this build, or a ScreenExtend is already running)' : ''}`);
    }
    if (dataDir) R.host.isolationVerified = fs.existsSync(path.join(dataDir, 'screenextend.lock'));
    // A `serve` without g09's "control server before adapter servers" fix answers /health ~300 ms before its control server exists; ask
    // again for a moment rather than deciding "no control verbs" (which means a PID stop later).
    let st = await hostJoins();
    for (let i = 0; i < 10 && !st.telemetry && build.controlStats; i++) { await sleep(200); st = await hostJoins(); }
    R.host.statusJson = st.available ? 'telemetry' : (st.status ? 'status only (no telemetry)' : 'unavailable');
    R.host.controlVerbs = !!st.telemetry;

    const cl = await openClient(REQ.client);
    R.chrome = { version: cl.chrome.version, flags: cl.chrome.args.filter((a) => !/^--user-data-dir|^about:/.test(a)) };
    const body = async () => {
      R.clock = await clockOffset(cl);
      if (REQ.calibrate) R.calibration = await calibrate(cl, REQ.client);
      R.scenarios = await standardScenarios(cl, REQ.client);
      if (REQ.mic_prompt) R.special.mic_prompt = await micPrompt();
      if (REQ.two_clients) R.special.two_clients = await twoClients(cl);
      if (REQ.leave_rejoin) R.special.leave_rejoin = await leaveRejoin(cl);
      if (REQ.slow_answer) R.special.slow_answer = await slowAnswer(cl);
      if (REQ.fallback_paths) R.special.fallback_paths = await fallbackPaths();
      if (REQ.mover_stop) R.special.mover_stop = await moverStopTest(cl);
      if (REQ.auto_rejoin) R.special.auto_rejoin = await autoRejoin(cl);
    };
    // By default the lock is held for the whole live-display part of the test, so no other
    // test's virtual display exists while this one measures: with two or more virtual displays
    // attached, streams on this build were seen to freeze, retarget or slow down (README,
    // "Concurrency"). Host start, browser start and teardown still overlap. `overlap_displays`
    // restores per-window locking, for a caller who wants that and accepts the risk.
    // The WUDFHost leak per display: snapshots at both ends of the live-display part, under the
    // lock, so the delta is this run's own (no other test creates a display while it is held).
    const memBody = async () => {
      const before = await memSnapshot();
      try { await body(); } finally {
        const after = await memSnapshot();
        const joins = (R.joins || []).length + (R.special.two_clients ? 1 : 0);
        const d = (k) => (after[k] != null && before[k] != null ? +(after[k] - before[k]).toFixed(1) : null);
        R.memory = { before, after, joinsInWindow: joins, wudfCommitDeltaMB: d('wudfCommitMB'), wudfHandlesDelta: d('wudfHandles'), wudfThreadsDelta: d('wudfThreads'), note: 'largest WUDFHost (virtual display driver host) before and after the live-display part; joins counts joins[] (+1 for a two_clients second browser); renegotiations and fallback tiers that reuse a display are not counted separately' };
      }
    };
    if (REQ.overlap_displays) await memBody(); else await underLock('live-display', memBody);
    if (REQ.pipe_close) R.special.pipe_close = await pipeClose();
    if (REQ.loss) { R.special.loss = { notRun: true, reason: 'inducing packet loss on this machine needs admin rights (a WFP/clumsy-style driver or netsh) or touching the owner\'s network; neither is allowed. Not run.' }; R.notRun.push('loss'); }
    R.extraJoinsSeen = (await hostJoins()).joins;
    R.pageConsole = { warningsAndErrors: cl.console.filter((c) => /warn|error|exception/.test(c.level)).slice(0, 200), total: cl.console.length };
    R.ok = true;
  } catch (e) {
    R.errors.push('harness: ' + (e.stack || e.message));
  } finally {
    R.cleanup = await cleanup();
    if (cfgBefore) {
      let after = sha(REAL_CONFIG);
      if (after !== cfgBefore) { fs.copyFileSync(path.join(RUN_DIR, 'config.json.before'), REAL_CONFIG); after = sha(REAL_CONFIG); R.ownerConfig.restored = true; }
      R.ownerConfig.sha256After = after; R.ownerConfig.identical = after === cfgBefore;
      if (!R.ownerConfig.identical) R.errors.push('owner config.json differs after the run and could not be restored');
    }
    releaseSlots(slots);
  }
  return R.ok ? 0 : 1;
}

async function cleanup() {
  const c = { stages: 0, clients: [], hosts: [] };
  for (const s of owned.stages) if (s.exitCode === null && pidAlive(s.pid)) { try { s.kill(); c.stages++; } catch { /* gone */ } }
  for (const cl of owned.clients) c.clients.push(await cl.close().catch((e) => ({ error: e.message })));
  for (const h of owned.hosts) c.hosts.push(await underLock('host-stop', () => stopHost(h, { exe: HOSTCTX.exe, env: HOSTCTX.dataDir ? { SCREENEXTEND_DATA_DIR: HOSTCTX.dataDir } : {}, useStopVerb: !!(R.host && R.host.controlVerbs) })));
  // Displays: whatever this run added must be gone.
  await sleep(1500);
  const st = await driverState();
  if (Array.isArray(st)) {
    const base = new Set((HOSTCTX && HOSTCTX.driverBefore || []).map((m) => m.id));
    let left = st.filter((m) => !base.has(m.id));
    if (HOSTCTX && HOSTCTX.mode === 'isolated') left = left.filter((m) => m.name && m.name.startsWith(`SEH${R.slot}`));
    c.displaysLeftByUs = left.map((m) => ({ id: m.id, name: m.name }));
    if (left.length && HOSTCTX && HOSTCTX.mode === 'exclusive') {
      // A build without a reachable stop verb was stopped by PID and could not remove its display.
      await driverRemove(left.map((m) => m.id)).catch((e) => { c.driverRemoveError = e.message; });
      await sleep(2500);
      const again = await driverState();
      c.driverRemoved = left.map((m) => m.id);
      c.displaysLeftAfterDriverRemove = Array.isArray(again) ? again.filter((m) => !base.has(m.id)).map((m) => m.id) : again;
    }
    c.driverAfter = st.map((m) => ({ id: m.id, name: m.name }));
  } else c.driverAfter = st;
  c.hostProcessesLeft = owned.hosts.filter((h) => pidAlive(h.pid)).map((h) => h.pid);
  c.chromeLeft = owned.clients.filter((cl) => cl.chrome && !cl.chrome.exited).map((cl) => cl.chrome.proc.pid);
  return c;
}

const code = await main().catch((e) => { R.errors.push('fatal: ' + (e.stack || e.message)); return 1; });
R.finishedAt = new Date().toISOString();
if (RUN_DIR) { try { fs.writeFileSync(path.join(RUN_DIR, 'handler.log'), LOGS.join('\n')); fs.writeFileSync(path.join(RUN_DIR, 'result.json'), JSON.stringify(R, null, 1)); } catch { /* best effort */ } }
if (!STATUS_ONLY) process.stdout.write(JSON.stringify(R, null, 1) + '\n');
process.exitCode = code;
