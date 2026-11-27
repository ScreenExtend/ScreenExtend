#!/usr/bin/env node
// ScreenExtend test handler; README.md has the contract.
//
//   node scripts/test-handler/handler.mjs --exe <ScreenExtend.exe> [--request <file.json | inline JSON>]
//
// Prints one JSON document on stdout. Exit 0 = the harness ran to completion (whatever the
// numbers say); non-zero = the harness itself failed, with the reason in the JSON.
// Never: inject input by any route, kill a pid this run did not start, or leave the owner's
// config.json changed.
import { spawn, execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ROOT, acquireSlot, acquireAll, releaseSlots, measureLock, pidAlive, slotStatus } from './lib/slots.mjs';
import { probeBuild, startHost, stopHost, runCli, cliJson, randomSessionId, randomOtp } from './lib/host.mjs';
import { Client, stampMetrics } from './lib/client.mjs';
import { preciseNow, anchorInfo } from './lib/clock.mjs';
import { driverState, driverRemove } from './lib/driver.mjs';
import { startLoadSampler, snapshot, waitForQuiet, memSnapshot } from './lib/load.mjs';
import { dist, spread } from './lib/metrics.mjs';
import { sleep } from './lib/cdp.mjs';
import { RX_HOOK_SOURCE, RX_ARM, RX_READ, rxRows, analyzeRx } from './lib/rx-hook.mjs';
import { Relay, QUEUE_DISCIPLINE } from './lib/relay.mjs';
import { probeRemote, startRemoteChrome } from './lib/remote.mjs';
import { procSample, slope } from './lib/proc.mjs';
import { reap } from './lib/reap.mjs';
import { readLog, joinStages, distOf, STAGE_KEYS, setCapturePatterns } from './lib/joinlog.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const RUN_ROOT = process.env.SE_TEST_RUN_ROOT || ROOT;
const VERSION = 1;

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
  arms: null,              // {arms: {name: {SE_X_*: v}}, order: [names], scenario, qualities: [q1, q2], settle_s}: A/B inside one join (see armsTest)
  join_failures: false,    // true | [cases] | {cases, stop_after_ms, watch_ms}: wrong_otp, unknown_session, host_stopped, host_error (a host made to answer 5xx)
  settings_only: null,     // array of settings_change edit labels to run (e.g. ["video quality"]); null = all
  mover_stop: false,       // true or {cycles, move_s, hold_s, step_ms, speed}: where does the picture come to rest?
  cpu_burn: null,          // {threads, seconds}: saturate the CPU during slow_answer's joins (inside the measurement lock)
  audio_paths: false,      // true = every client audio tier; or a list of tier keys (AUDIO_TIERS)
  audio_keep_page_mute: false, // do not unmute a page that muted itself before the click train
  click_source: 'winmm',   // 'wasapi': clicks timed by the engine's play position (clicks-wasapi.ps1)
  min_commit_gb: 3,        // wait before starting until this much system commit is free (0 = off)
  netlog: false,           // Chrome's own network log per browser, into the run directory
  stamp_band: 'full',      // 'full': average each stamp cell's whole height; 'center': a 2-row band (immune to stage tearing)
  // After every join has left: close the host's stdout/stderr pipes (as a parent that stops
  // reading does), make it log, and report whether its control server still answers.
  pipe_close: false,
  // Extra query parameters appended to every join URL (e.g. a page's own debug switches), and
  // an expression evaluated in the page after every join whose JSON result lands in that join's
  // record as `probe` (e.g. a timeline the page keeps). Both are inert for a page that ignores them.
  url_params: '', join_probe: null,
  // Before every timed window: wait up to max_wait_s for whole-machine CPU (2 s average) to fall
  // below max_cpu. A window that opened loaded, or whose own mean CPU ended above max_cpu, is
  // marked `loaded: true` and should be discarded and repeated. `load_gate: false` turns it off.
  load_gate: { max_cpu: 75, max_wait_s: 180 },
  // {rates, browsers, seconds, settle_s, large, client, relay}: when does the transform worker get each frame? See receiveHold.
  receive_hold: null,
  // Network impairment through the loopback relay: true, or {scenarios, baseline_s, impaired_s, recover_s, large, client, browser}; see impairTest.
  impair: false,
  // Slow clients: true, or {cases: [worker_spin, cpu_throttle, hidden, software_decode], spin_ms, cpu_rate, baseline_s, slow_s, recover_s}; see slowClient.
  slow_client: false,
  // A browser on another machine as the client: {ssh, rates, seconds, chrome_path, flags}; ssh may come from SE_TEST_REMOTE_CLIENT.
  remote_client: false,
  // Several browsers on one host, added one at a time: true or {max (<= 4), seconds, client}.
  multi_session: false,
  // Join, leave, rejoin, renegotiate, aborted join, killed browser: true or {kinds, cycles, client}.
  churn: false,
  // One long session with a series every period_s: true or {minutes, period_s, large, mover, client}.
  soak: false,
  // Real cursor and injection: needs allow_real_input: true as well. {cases: [cursor_sweep, remote_control], seconds, moves, client}.
  allow_real_input: false, real_input: false,
  // {joins, threshold_ms, race, pause_between_s, cpu_burn, browser, client, pause_reader_ms}: see joinReliability.
  join_reliability: false,
  display_check: false,
  abandon_join: false,
  // 'relay': a separate process reads the host's output (lib/tslog.mjs), so this loop stalling
  // (1.5 to 5 s per join, synchronous PowerShell calls) cannot block the host's log writes.
  // 'pipe': read in this process.
  host_log: 'relay',
};
const KNOWN = new Set([...Object.keys(DEFAULTS), 'label', 'notes']);

/** Anything that would reach the host's input injector is refused, with the reason. */
function inputRefusal(req) {
  // real input is opt-in by the caller, and only through these two keys
  const optIn = req.allow_real_input === true;
  const keys = Object.keys(req).filter((k) => /input|mouse|keyboard|pointer|touch|cursor|key_?press|clipboard|wheel/i.test(k) && !(optIn && (k === 'allow_real_input' || k === 'real_input')));
  if (keys.length) return `request keys ${JSON.stringify(keys)} ask for input; input injection is forbidden (the owner's cursor and keyboard are off limits)`;
  const js = [req.custom && typeof req.custom.js === 'string' ? req.custom.js : '', typeof req.join_probe === 'string' ? req.join_probe : '', typeof req.url_params === 'string' ? req.url_params : ''].join(' ');
  const banned = /(new\s+(Mouse|Pointer|Keyboard|Wheel|Touch|Input|Focus|Drag|Clipboard)Event)|dispatchEvent|Input\.dispatch|\.focus\(|requestPointerLock|keyboard\.lock|initMouseEvent|createEvent|RemoteInput|sendInput/i;
  if (js.trim() && banned.test(js)) return 'custom.js / join_probe / url_params creates or dispatches events (or touches RemoteInput); the client page forwards such events to the host, which would move the real cursor. Refused.';
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
    else if (argv[i] === '--reap') a.reap = true;
    else if (argv[i] === '--gate-check') a.gateCheck = Number(argv[++i]) || 2;
    else if (argv[i] === '--parent-pid') a.parentPid = Number(argv[++i]);
    else if (argv[i] === '--calibrate-only') a.calibrateOnly = true;
    else if (argv[i] === '--help' || argv[i] === '-h') a.help = true;
  }
  return a;
}

const R = { handler: VERSION, startedAt: new Date().toISOString(), ok: false, warnings: [], errors: [], notRun: [], measurementWindows: [] };
const LOGS = [];
let RUN_DIR = null;
const log = (s) => { const line = `${new Date().toISOString().slice(11, 23)} ${s}`; LOGS.push(line); process.stderr.write(line + '\n'); };
const owned = { hosts: [], clients: [], stages: [], relays: [] }; // everything to tear down, by PID

const REAL_CONFIG = path.join(process.env.APPDATA || '', 'app.screenextend.desktop', 'config.json');
const sha = (f) => { try { return crypto.createHash('sha256').update(fs.readFileSync(f)).digest('hex'); } catch { return null; } };

// every timed window goes through here, so the lock is held for exactly that window
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

// Which \\.\DISPLAYn carries the display this join made. The bundled driver gives every
// monitor the EDID name "VirtuDisplay+", so names cannot be used: ask the host (current builds
// report each join's display device), else take the one screen that appeared since host start.
async function resolveGdi(name, { quiet = false } = {}) {
  const j = await hostJoins();
  if (quiet && !(j.joins || []).some((x) => x.name === name)) return { error: 'no join yet' };
  if (j.joins) {
    const mine = j.joins.filter((x) => x.name === name && x.display && x.display.device);
    if (mine.length === 1) return { gdi: mine[0].display.device, driverId: mine[0].display.id, via: 'host status --json (driver id -> device path)' };
    // two live joins for one browser (a retried offer): the newest is the one the page shows
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

// every stage's once-a-second report: stamps drawn and the monitor its window is on
const STAGE_TICKS = [];
// every 250 ms from each stage: non-primary monitors by handle, device path and rectangle, plus display messages
const STAGE_MONS = [];
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
  let tickBuf = '';
  proc.stdout.on('data', (d) => {
    const lines = (tickBuf + d).split(/\r?\n/); tickBuf = lines.pop();
    for (const l of lines) {
      if (l.includes('"mon"')) { try { const j = JSON.parse(l); if (STAGE_MONS.length < 100000) STAGE_MONS.push({ ...j, pid: proc.pid }); } catch { /* partial */ } continue; }
      if (!l.includes('"tick"')) continue; try { const j = JSON.parse(l); if (STAGE_TICKS.length < 50000) STAGE_TICKS.push({ ...j, pid: proc.pid }); } catch { /* partial */ }
    }
  });
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

let H = null;          // the current host handle
let HOSTCTX = null;    // { exe, dataDir, env, ports, sessionId, otp, build, mode }

async function bootHost(label) {
  const c = HOSTCTX;
  // Display-topology changes (a create, a removal, a startup reclaim) stall every display's
  // compositor for a moment, so anything that can make one runs under the measurement lock too.
  const h = await underLock('host-start', () => startHost({ exe: c.exe, dataDir: c.dataDir, httpPort: c.ports.http, httpsPort: c.ports.https, sessionId: c.sessionId, otp: c.otp, envCredentials: c.build.envCredentials, extraArgs: c.hostArgs, extraEnv: c.hostEnv, logFile: path.join(RUN_DIR, 'serve.log'), logRelay: c.logRelay }));
  owned.hosts.push(h);
  log(`host ${label}: pid ${h.pid} up=${h.up} ip=${h.ip} in ${h.startupMs} ms`);
  return h;
}

const cliEnv = () => (HOSTCTX.dataDir ? { SCREENEXTEND_DATA_DIR: HOSTCTX.dataDir } : {});
const cli = (args, timeout) => gatedCli(args, timeout);

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
    // shared-driver mode records each monitor's owner pid (0 = released, still attached)
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

async function deviceKey(name, cl) {
  // By token where the build reports one (devices set refuses an address two devices share);
  // older builds only know addresses. A retried join leaves two joins with one name and two
  // tokens, in no useful order: the join id the page holds decides, else the last same-named.
  const j = await hostJoins();
  if (j.joins) {
    const mine = j.joins.filter((x) => x.name === name);
    let held = null;
    if (cl) held = await cl.page.ev('(() => { try { return sessionStorage.getItem("se.joinId") || ""; } catch (e) { return ""; } })()').catch(() => null);
    const live = held ? mine.find((x) => x.joinId === held) : null;
    if (live) return { key: live.token, by: 'token (the join the page holds)', joins: j.joins };
    if (mine.length) return { key: mine[mine.length - 1].token, by: 'token', joins: j.joins };
  }
  return { key: H.ip, by: 'ip (this build reports no join tokens)', joins: null };
}

function clientUrl(c, name, { https = true } = {}) {
  const origin = https ? `https://${H.ip}:${HOSTCTX.ports.https}` : `http://${H.ip}:${HOSTCTX.ports.http}`;
  const extra = REQ.url_params ? '&' + String(REQ.url_params).replace(/^[?&]+/, '') : '';
  return { origin, url: `${origin}/?id=${HOSTCTX.sessionId}&name=${encodeURIComponent(name)}&otp=${HOSTCTX.otp}&fs=0${extra}` };
}

const STD_GRANT = ['audioCapture', 'wakeLockScreen', 'speakerSelection'];
let CLIENT_SEQ = 0, PROFILE_SEQ = 0;
const deviceName = () => `SEH${R.slot}${crypto.randomBytes(2).toString('hex').toUpperCase()}${++CLIENT_SEQ}`.slice(0, 13);

async function openClient(spec, { flags = [], disable = [], offset = 0, browser = 'chrome', workerExtra = '', headed = null } = {}) {
  const port = 19200 + R.slot * 10 + offset;
  // a fresh directory per browser: a profile Chrome has just let go of can stay locked for a while
  const profile = path.join(RUN_ROOT, `profile-slot${R.slot}-${offset}-${process.pid}-${++PROFILE_SEQ}`);
  const netlog = REQ.netlog && RUN_DIR ? [`--log-net-log=${path.join(RUN_DIR, `netlog-${offset}-${PROFILE_SEQ}.json`)}`] : [];
  const cl = new Client({ port, profile, flags: [...REQ.chrome_flags, ...flags, ...netlog], headed: headed == null ? REQ.headed : headed, log, scWidth: REQ.screencast_width, browser, workerExtra, gate: displayGate });
  cl.stampBand = REQ.stamp_band; owned.clients.push(cl);
  await cl.open({ client: spec, observer: { refreshRate: spec.refresh, disable } });
  return cl;
}

async function joinClient(cl, spec, { name = deviceName(), grant = STD_GRANT, micChoice = 'continue', submitAt = 0, prepared = false } = {}) {
  const { origin, url } = clientUrl(spec, name, { https: spec.https !== false });
  // A stage waiting before the submit draws its first stamp the moment the join's display
  // attaches; the first frame showing it is the first painted frame. A rejoin that keeps its
  // display draws on that one.
  // a timed rejoin skips the CLI lookup: it costs seconds, and the display it would find is going away
  const pre = prepared ? { error: "skipped" } : await resolveGdi(name, { quiet: true });
  const joinT0 = Date.now();
  const j = await underLock('join', async () => {
    const known = screens();
    const factory = () => (pre.gdi ? startStage(pre.gdi, { seconds: 45, wait: 30, driverId: pre.driverId }) : startStage('', { seconds: 45, notIn: known, wait: 30 }));
    const restage = async (old) => {
      stopStage(old); const r = await resolveGdi(name, { quiet: true });
      if (r.error) {
        const n = screens().filter((x) => !known.includes(x));
        if (n.length) return startStage(n[n.length - 1], { seconds: 45, wait: 10 });
        // A build that reports no joins (v0.6.3) and reused the display it kept through the
        // disconnect grace: no new screen comes with this join; the one added since host start is it.
        const added = screens().filter((x) => !(HOSTCTX.screensBefore || []).includes(x));
        return added.length === 1 ? startStage(added[0], { seconds: 45, wait: 10 }) : null;
      }
      return startStage(r.gdi, { seconds: 45, wait: 10, driverId: r.driverId });
    };
    try { return await cl.join({ url, origin, grant, micChoice, stage: factory, restage, submitAt, prepared }); } finally { stopStage(cl.joinStage); cl.joinStage = null; }
  });
  j.name = name;
  // the monitor list only when it changed, and every display message
  { let last = null; j.monitorChanges = []; j.displayMessages = [];
    for (const m of STAGE_MONS.filter((x) => x.mon >= joinT0 - 1000)) {
      const key = JSON.stringify(m.list); if (key !== last) { j.monitorChanges.push({ t: m.mon, list: m.list }); last = key; }
      for (const msg of m.msgs || []) j.displayMessages.push({ msg: msg[0] === 126 ? 'WM_DISPLAYCHANGE' : msg[0] === 537 ? 'WM_DEVICECHANGE' : msg[0] === 736 ? 'WM_DPICHANGED' : 'WM_SETTINGCHANGE', t: msg[1], wparam: msg[2], from: m.pid });
    } }
  j.stageTicks = STAGE_TICKS.filter((x) => x.t >= joinT0 - 1000).map((x) => ({ t: x.t, s: x.tick, draws: x.draws, pid: x.pid, gdi: x.where ? x.where.gdi : null, path: x.where && x.where.path ? x.where.path.split('#').slice(1, 3).join('#') : null, cloaked: x.where ? x.where.cloaked : null, visible: x.where ? x.where.visible : null }));
  if (REQ.join_probe) j.probe = await cl.page.ev(`(() => { try { return JSON.stringify(${REQ.join_probe}); } catch (e) { return JSON.stringify({ error: String(e) }); } })()`).then((v) => { try { return JSON.parse(v); } catch { return v; } }, (e) => ({ error: e.message }));
  log(`join ${name}: answer ${j.answerMs} ms, first picture ${j.firstPictureMs} ms${j.error ? ' ERROR ' + j.error : ''}${j.pageError ? ' page: ' + j.pageError : ''}`);
  return j;
}

async function leave(cl) {
  // under the lock: the leave removes a display (after the host's settle delay)
  await underLock('leave', async () => {
    const before = await ourMonitors();
    await cl.away();
    const n0 = before.monitors ? before.monitors.length : 0;
    for (let i = 0; i < 16; i++) { await sleep(500); const m = await ourMonitors(); if (!m.monitors || m.monitors.length < n0 || n0 === 0) break; }
    await sleep(500);
  });
}

async function clockOffset(cl) {
  // browser `performance.timeOrigin + now()` minus the precise system clock (lib/clock.mjs),
  // from the minimum-RTT sample. This is the reference offset: every sample the client returns
  // is moved onto this clock first (Client.drainStamps measures each context's own offset).
  let best = null;
  for (let i = 0; i < 25; i++) {
    const t0 = preciseNow();
    const b = await cl.page.ev('performance.timeOrigin + performance.now()');
    const t1 = preciseNow();
    const s = { rtt: t1 - t0, off: b - (t0 + t1) / 2 };
    if (!best || s.rtt < best.rtt) best = s;
  }
  return { browserMinusSystemMs: Math.round(best.off * 100) / 100, rttMs: Math.round(best.rtt * 100) / 100, anchor: anchorInfo, method: 'min-RTT of 25 Runtime.evaluate round trips against the precise system clock (QPC anchored to GetSystemTimePreciseAsFileTime, lib/clock.mjs)' };
}

async function calibrate(cl, spec) {
  // The method's own floor: a local page (no stream) draws the stamp into a canvas, wraps it in
  // a VideoFrame and paints it with drawImage at 60 Hz, as the client page does with a decoded
  // frame. What is left is readback error and stamp quantisation: no host, no network.
  await cl.away(); await sleep(300);
  await cl.page.ev(`(() => {
    document.body.style.margin = 0; document.body.style.background = '#000';
    const src = new OffscreenCanvas(${spec.width}, ${spec.height}); const g = src.getContext('2d');
    const cv = document.createElement('canvas'); cv.width = ${spec.width}; cv.height = ${spec.height};
    cv.style.cssText = 'display:block;width:100vw;height:100vh'; document.body.appendChild(cv);
    const out = cv.getContext('2d'); const cell = Math.max(8, Math.min(64, Math.floor(${spec.width} / 40)));
    g.fillStyle = '#336'; g.fillRect(0, 0, ${spec.width}, ${spec.height});
    setInterval(() => {
      const ms = Math.round(performance.timeOrigin + performance.now() - (window.__seCalOff || 0)); const v = ms & 0xFFFFFF; const chk = ((((v >> 16) & 255) + ((v >> 8) & 255) + (v & 255)) & 255) ^ 0x5A;
      const bits = [1, 0, 1, 0]; for (let i = 23; i >= 0; i--) bits.push((v >> i) & 1); for (let i = 7; i >= 0; i--) bits.push((chk >> i) & 1); bits.push(0, 1);
      bits.forEach((b, i) => { g.fillStyle = b ? '#fff' : '#000'; g.fillRect(i * cell, 0, cell, cell); });
      const f = new VideoFrame(src, { timestamp: ms * 1000 }); out.drawImage(f, 0, 0); f.close();
    }, 16); })()`);
  await sleep(500);
  // The page's Date.now() is the coarse wall clock (lib/clock.mjs); draw the stamp from this
  // document's own precise clock instead, so the floor measures the method and not that.
  const po = await cl.contextOffset(null);
  await cl.page.ev(`window.__seCalOff = ${po ? po.off : 0}`);
  const cell = Math.max(8, Math.min(64, Math.floor(spec.width / 40)));
  const stage = { width: spec.width, height: spec.height, cell, cellH: cell };
  return underLock('calibration', async () => {
    const { samples, counters } = await cl.collectStamps(stage, 6000);
    // samples come back on the reference clock (Client.drainStamps), so the stamp is compared
    // after removing the same reference offset every stream window removes
    const m = stampMetrics(samples, { from: -Infinity, to: Infinity, offsetMs: Client.refOffsetMs ?? 0 });
    m.windowSeconds = 6; m.paintedFps = Math.round((m.newStamps / 6) * 10) / 10; m.hookCounters = counters;
    await cl.away();
    return { ...m, note: 'local page, no stream: a stamp from the page\'s own Date.now() painted as a VideoFrame through drawImage every 16 ms and read back by the same paint hook. This is the floor of the method (readback error + 1 ms stamp quantisation); it excludes the client compositor, exactly as every stream measurement does.' };
  });
}

async function loadGate(label) {
  const g = REQ.load_gate;
  if (!g) return null;
  const r = await waitForQuiet({ maxCpu: g.max_cpu ?? 75, maxWaitMs: (g.max_wait_s ?? 180) * 1000 });
  log(`load gate (${label}): ${r.loaded ? 'still loaded' : 'quiet'} after ${r.waitedMs} ms, cpu ${r.cpuAtOpen} %`);
  return r;
}
// "loaded": the gate gave up, or the window's own mean CPU ended above the bar
function loadedVerdict(gate, loadSum) {
  const bar = REQ.load_gate ? (REQ.load_gate.max_cpu ?? 75) : null;
  const mean = loadSum && loadSum.cpuPct ? loadSum.cpuPct.mean : null;
  return { loaded: !!((gate && gate.loaded) || (bar != null && mean != null && mean > bar)), loadGate: gate, windowCpuMean: mean };
}

async function timedWindow(cl, name, { seconds, mover, large, label }) {
  // The gate runs before the stage starts: the stage process lives for a fixed time, and a
  // gate that waited inside the window would outlast it.
  const gate = await loadGate(label);
  // the noise is painted by its own process, so a slow paint can never hold the stamp back
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
    return { ...m, ...loadedVerdict(gate, loadSum), clockContexts: cl.clockContexts(), stage: ready, stageDone: st.done(), layout: got.layout, load: loadSum, secondaryBefore: secBefore, secondaryAfter: secAfter, pageCounters: pageDelta(secBefore, secAfter, seconds) };
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

async function standardScenarios(cl, spec) {
  const out = {};
  const want = (s) => REQ.scenarios.includes(s);
  const common = ['static', 'small_window', 'large_area', 'stamp_only'].filter(want);
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
      for (const [sc, mover, large] of [['small_window', true, false], ['large_area', false, true], ['stamp_only', false, false]]) {
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
    // A fresh device identity for the hi-res join: the common join left a device token in
    // localStorage and the host saved a row under it (refreshRate 60), which would pin the
    // "120 Hz" scenario to 60 Hz.
    if (H) { const o = clientUrl(hs, 'x', { https: hs.https !== false }).origin; R.hiresStorageCleared = await cl.page.send('Storage.clearDataForOrigin', { origin: o, storageTypes: 'local_storage,cookies' }).then(() => true, (e) => e.message); }
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

async function audioTest(cl, name, { beforeClicks = null } = {}) {
  const res = { };
  const k = await deviceKey(name, cl);
  const set = await cli(['devices', 'set', k.key, '--audio', 'on']);
  res.enable = { by: k.by, code: set.code, out: (set.stdout + set.stderr).trim().slice(0, 300) };
  if (HOSTCTX.mode === 'exclusive') res.enable.note = 'wrote the real config.json; it is restored byte-for-byte at the end of the run';
  await sleep(4000); // the first enable renegotiates once
  if (beforeClicks) res.beforeClicks = await beforeClicks(k).catch((e) => ({ error: e.message }));
  if (/restart it or reconnect/i.test(res.enable.out)) {
    // Older builds apply a saved override only to a new connection, and v0.6.3's `serve`
    // reads device overrides from config.json only at startup: there the host is restarted
    // (same credentials and ports) before the rejoin.
    await leave(cl);
    if (!R.host.controlVerbs) {
      res.hostRestart = await stopHostGated(H, { exe: HOSTCTX.exe, env: cliEnv(), useStopVerb: false });
      H = await bootHost('restart-for-audio');
    }
    res.rejoin = await joinClient(cl, REQ.client, { name });
    res.enable.note = (res.enable.note ? res.enable.note + '; ' : '') + (R.host.controlVerbs ? 'this build applies the override on reconnect, so the device left and rejoined' : 'this build reads overrides only at serve startup, so the host was restarted and the device rejoined');
    await sleep(2000);
  }
  return underLock('audio', async () => {
    // Every handler client runs on the host machine, which a page may treat as a reason to
    // keep its own playback muted (the same-machine echo). Chrome is muted here anyway, so the
    // page is unmuted through its own API and the graph is measured; reported either way.
    res.pageMutedBeforeTest = await cl.page.ev(`(window.SEAudio && SEAudio.isMuted) ? (SEAudio.isMuted() ? (${REQ.audio_keep_page_mute ? '' : 'SEAudio.setMuted(false), '}true) : false) : null`).catch(() => null);
    res.pageUnmutedByHandler = !!res.pageMutedBeforeTest && !REQ.audio_keep_page_mute;
    await cl.page.ev('__se.startOnsets(0.02, 30)');
    await cl.drainAudioDecodeOnsets().catch(() => null);
    await sleep(2500);
    const before = await cl.page.ev('__se.audioBuckets.slice()');
    const clicks = await new Promise((resolve) => {
      const script = REQ.click_source === 'wasapi' ? 'clicks-wasapi.ps1' : 'clicks.ps1';
      const p = spawn('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(HERE, script), '-Seconds', '12'], { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
      owned.stages.push(p); let o = ''; p.stdout.on('data', (d) => { o += d; }); p.stderr.on('data', (d) => { o += d; });
      p.on('exit', () => { try { resolve(JSON.parse(o.trim().split('\n').pop())); } catch { resolve({ error: o.slice(-400) }); } });
    });
    await sleep(2500);
    const page = await cl.page.ev('({ onsets: __se.onsets.slice(), buckets: __se.audioBuckets.slice(), analysers: __se.analysers.length, ctx: __se.analysers.map((a) => ({ state: a.ctx.state, sr: a.ctx.sampleRate, outputLatencyMs: a.ctx.outputLatency * 1000, baseLatencyMs: a.ctx.baseLatency * 1000 })) })');
    await cl.page.ev('__se.stopOnsets()');
    const sec = await cl.secondary();
    res.clicksPlayed = clicks.clicks ? clicks.clicks.length : 0; res.player = { ...clicks, clicks: undefined };
    res.analysers = page.analysers; res.audioContexts = page.ctx;
    // The page's onsets are on this document's clock; its offset from the precise system clock
    // (the click times' clock) is measured now, not taken from the first document of the run.
    const po = await cl.contextOffset(null);
    const off = po ? po.off : R.clock.browserMinusSystemMs;
    res.pageClockOffsetMs = po ? Math.round(po.off * 100) / 100 : null;
    const toRender = [], toOutput = [], peaks = []; const matched = new Set();
    for (const c of clicks.clicks || []) {
      const hit = page.onsets.findIndex((o, i) => !matched.has(i) && o.render - off >= c - 20 && o.render - off <= c + 650);
      if (hit < 0) continue; matched.add(hit);
      const o = page.onsets[hit]; toRender.push(o.render - off - c); if (o.output) toOutput.push(o.output - off - c); peaks.push(o.peak);
    }
    // the same clicks at the decode worker's tap: splits the delay into host -> decoded and
    // decoded -> rendered
    const dec = await cl.drainAudioDecodeOnsets().catch(() => ({ onsets: [], packets: 0 }));
    const toDecode = [], decodeToRender = []; const dUsed = new Set();
    for (const c of clicks.clicks || []) {
      // `decoded` is already on the precise system clock (drainAudioDecodeOnsets)
      const di = dec.onsets.findIndex((o, i) => !dUsed.has(i) && o.decoded >= c - 20 && o.decoded <= c + 650);
      if (di < 0) continue; dUsed.add(di); toDecode.push(dec.onsets[di].decoded - c);
      const ri = page.onsets.findIndex((o) => o.render - off >= dec.onsets[di].decoded - 5 && o.render - off <= c + 650);
      if (ri >= 0) decodeToRender.push(page.onsets[ri].render - off - dec.onsets[di].decoded);
    }
    res.delayToDecodeMs = dist(toDecode); res.decodeToRenderMs = dist(decodeToRender); res.decodedPackets = dec.packets; res.decodedOnsets = dec.onsets.length;
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
    res.memory = commitHeadroomGb();
    res.mediaElements = await cl.page.ev('__se.mediaSummary ? __se.mediaSummary() : null').catch((e) => ({ error: e.message }));
    res.analyserKinds = await cl.page.ev('__se.analysers.map((a) => a.kind || "graph")').catch(() => null);
    res.definitions = { host: 'click time = when winmm reported the click\'s first sample consumed (clicks.ps1)', render: 'onset found by an analyser spliced before the destination, timed by AudioContext.currentTime', output: 'render time mapped through getOutputTimestamp (adds the browser\'s output latency)', note: 'Chrome runs with --mute-audio so the host does not capture the client\'s playback; the graph still renders. Other sounds on the machine are captured too and show up as unmatched onsets.' };
    return res;
  });
}

async function settingsChange(cl, name) {
  const out = { edits: [] };
  const k0 = await deviceKey(name, cl);
  const edits = [
    ['audio on', ['--audio', 'on']], ['audio off', ['--audio', 'off']],
    ['video quality', ['--video-quality', '30']], ['video scale', ['--video-scale', '75']],
    ['refresh rate', ['--refresh-rate', '120']], ['DPR (native-dpr off)', ['--native-dpr', 'off']],
    ['orientation portrait', ['--orientation', 'Portrait']], ['orientation landscape', ['--orientation', 'Landscape']],
    ['display scale', ['--scale', '150']], ['name', null], ['adaptive quality', ['--adaptive-quality', 'on']],
  ];
  for (const [label, args] of edits.filter(([l]) => !REQ.settings_only || REQ.settings_only.includes(l))) {
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

// A/B arms inside one join, interleaved on one display: no join-to-join variance, no display
// creation per arm. Each step writes the arm's switches to `SE_X_SWITCH_FILE` (a build that
// does not read it ignores them), renegotiates in place (encoder-only edit) and takes one timed
// window. Video quality alternates per step: a balanced order (A B B A) gives each arm both values.
async function armsTest(cl) {
  const o = REQ.arms;
  const arms = o.arms || {};
  const order = o.order || [];
  const qualities = o.qualities || [20, 21];
  const sc = o.scenario || 'small_window';
  const hires = /^hires/.test(sc);
  const spec = hires ? { ...REQ.hires_client } : REQ.client;
  const file = HOSTCTX.hostEnv.SE_X_SWITCH_FILE;
  const write = (sw) => fs.writeFileSync(file, Object.entries(sw || {}).map(([k, v]) => `${k}=${v}`).join('\n') + '\n');
  write(arms[order[0]]);
  if (hires) {
    await cl.setClient(spec);
    const origin = clientUrl(spec, 'x', { https: spec.https !== false }).origin;
    await cl.page.send('Storage.clearDataForOrigin', { origin, storageTypes: 'local_storage,cookies' }).catch(() => {});
  }
  const j = await joinClient(cl, spec);
  R.joins.push({ scenario: 'arms', ...j });
  const out = { scenario: sc, order, qualities, steps: [] };
  if (j.firstPictureMs == null) { out.error = 'no picture after join'; await leave(cl); if (hires) await cl.setClient(REQ.client); return out; }
  const k = await deviceKey(j.name, cl);
  for (let i = 0; i < order.length; i++) {
    const arm = order[i]; const q = qualities[i % qualities.length];
    write(arms[arm]);
    const before = { monitors: await ourMonitors(), pcs: await cl.page.ev('__se.pcs.length').catch(() => null) };
    // `set_args` ride along on every edit. A device with no saved row gets one written by the CLI
    // with refreshRate 60 (cli.rs `devices set` default), which pins a 120 Hz client to 60 fps:
    // pass ['--refresh-rate', '120'] for the hi-res client to keep it at its real rate.
    const cmd = await cli(['devices', 'set', k.key, '--video-quality', String(q), ...(o.set_args || [])]);
    await sleep((o.settle_s || 5) * 1000);
    const w = await timedWindow(cl, j.name, { seconds: REQ.duration, mover: !/large/.test(sc), large: /large/.test(sc), label: `arm:${arm}#${i + 1}` });
    const after = { monitors: await ourMonitors(), pcs: await cl.page.ev('__se.pcs.length').catch(() => null) };
    out.steps.push({ i: i + 1, arm, quality: q, cmd: cmd.code, newPeerConnections: after.pcs != null && before.pcs != null ? after.pcs - before.pcs : null, displays: after.monitors.monitors ? after.monitors.monitors.length : null, window: w });
  }
  const byArm = {};
  for (const s of out.steps) (byArm[s.arm] = byArm[s.arm] || []).push(s.window);
  out.aggregate = Object.fromEntries(Object.entries(byArm).map(([a, ws]) => [a, aggregate(ws)]));
  await leave(cl);
  if (hires) await cl.setClient(REQ.client);
  return out;
}

async function renameDevice(k, name) {
  const g = await cli(['config', 'get', 'devices']);
  let rows = null; try { rows = JSON.parse(g.stdout.slice(g.stdout.indexOf('['))); } catch { /* no rows */ }
  const i = rows ? rows.findIndex((r) => r.token === k.key || r.ip === k.key || r.name === name) : -1;
  if (i < 0) return { code: 2, stdout: '', stderr: 'no saved device row to rename (the audio edit creates one on the current build)' };
  return cli(['config', 'set', `devices.${i}.name`, name + 'R']);
}

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
  // `logs: {burst: N}`: N control connections with a wrong token, each logged once by the host
  // ("presented a bad token"). A log burst from outside the host over its loopback socket alone
  // (no input, no display), enough to wrap the 2000-line ring.
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

// The host's own record of what a join posted to `/audio-outputs`: the `[audio-outputs]` line
// `serve` logs per post. The last line for the join the page holds wins; polled briefly because
// the post follows the answer by a few ms. A build without the line reports `{ line: null }`.
async function hostAudioOutputs(cl) {
  const joinId = await cl.page.ev('(() => { try { return sessionStorage.getItem("se.joinId") || ""; } catch (e) { return ""; } })()').catch(() => '');
  if (!joinId) return { joinId: null, line: null, note: 'the page holds no join id' };
  for (let i = 0; i < 15; i++) {
    // authoritative: `status --json` telemetry.joins[].audioOutputs (builds from
    // `audio-outputs-supported` on), with `supported: null` until the page has posted
    const st = await cli(['status', '--json'], 10000);
    let held = null;
    try {
      const j = JSON.parse(st.stdout);
      const row = ((j.telemetry && j.telemetry.joins) || []).find((x) => x.joinId === joinId);
      held = row && row.audioOutputs ? row.audioOutputs : null;
    } catch { /* an older build or a non-JSON answer */ }
    const r = await cli(['logs', '--lines', '2000'], 10000);
    const lines = r.stdout.split(/\r?\n/).filter((l) => l.includes(`[audio-outputs] join ${joinId}:`));
    if (lines.length) {
      const last = lines[lines.length - 1];
      const f = (k) => { const m = last.match(new RegExp(k + '=(\\S+)')); return m ? m[1] : null; };
      const om = last.match(/outputs=(\d+) \(([^)]*)\)/);
      if (held && held.supported === null) { await sleep(200); continue; }
      return { joinId, posts: lines.length, supported: held ? String(held.supported) : f('supported'), outputs: om ? Number(om[1]) : null, kinds: om ? om[2] : null, labelled: f('labelled'), current: f('current'), statusJson: held, discovery: held ? held.discovery : f('discovery'), line: last.trim() };
    }
    await sleep(200);
  }
  return { joinId, line: null, note: 'no [audio-outputs] line for this join (no post, or a build without the line)' };
}

// `mic_prompt`: three fresh browsers (speaker access granted, microphone granted, nothing
// granted and the card answered with Skip). `{ rejoin: true }`: the nothing-granted browser
// joins again to show the answer is remembered. `{ audio: true }`: system audio on per device,
// then whether the page's audio graph is receiving and playing. `cases` narrows by label.
// Each case costs one display creation; the rejoin one more.
async function micPrompt() {
  const opt = typeof REQ.mic_prompt === 'object' ? REQ.mic_prompt : {};
  const out = {};
  let cases = [
    ['speaker access granted (no microphone)', ['speakerSelection', 'wakeLockScreen']],
    ['microphone granted', ['audioCapture', 'wakeLockScreen']],
    ['nothing granted', null],
    // Plain HTTP to the LAN address: not a secure context, so `navigator.mediaDevices`,
    // `setSinkId` and `selectAudioOutput` are all absent. Only with `http: true` (or by label).
    ['plain HTTP, nothing granted', null, { https: false }],
  ];
  if (Array.isArray(opt.cases)) cases = cases.filter(([l]) => opt.cases.includes(l));
  else if (!opt.http) cases = cases.filter(([, , o]) => !o);
  const readPage = (cl) => cl.page.ev(`(async () => ({
      gum: __se.gum, permQueries: __se.permQueries,
      secure: window.isSecureContext,
      micPermission: navigator.permissions ? await navigator.permissions.query({ name: 'microphone' }).then((p) => p.state, (e) => 'ERR ' + e.message) : 'no permissions API',
      outputs: navigator.mediaDevices ? await navigator.mediaDevices.enumerateDevices().then((d) => d.filter((x) => x.kind === 'audiooutput').map((x) => ({ idEmpty: !x.deviceId, label: x.label }))) : 'no navigator.mediaDevices',
      outputPlan: window.SEAudio && SEAudio.getOutputPlan ? SEAudio.getOutputPlan() : 'no getOutputPlan (older page)',
      stored: (() => { const o = {}; try { for (let i = 0; i < localStorage.length; i++) { const k = localStorage.key(i); if (/^se\\.outputs\\./.test(k)) o[k] = localStorage.getItem(k); } } catch (e) { o.error = e.message; } return o; })(),
      selectAudioOutput: !!(navigator.mediaDevices && navigator.mediaDevices.selectAudioOutput),
      setSinkId: 'setSinkId' in HTMLMediaElement.prototype }))()`).catch((e) => ({ error: e.message }));
  const summarise = (j) => {
    const posts = (j.whepRequests || []).filter((w) => w.method === 'POST');
    const outputsPost = (j.apiRequests || []).find((w) => w.method === 'POST' && /^\/audio-outputs/.test(w.url));
    return { modals: j.modals, submitToFirstPostMs: posts.length ? posts[0].sentMs : null, offers: posts.length, answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, audioOutputsPostedMs: outputsPost ? outputsPost.sentMs : null, pageError: j.pageError || '' };
  };
  for (const [label, grant, caseOpt] of cases) {
    const spec = { ...REQ.client, ...(caseOpt || {}) };
    const cl = await openClient(spec, { offset: 3 });
    const j = await joinClient(cl, spec, { grant, micChoice: 'skip' });
    const page = await readPage(cl);
    out[label] = { join: summarise(j), micCardShown: j.modals.some((m) => /^micCard/.test(m)), microphoneRequested: (page.gum || []).some((g) => /audio/.test(g.constraints)), ...page,
      outputsAvailableWithoutMic: Array.isArray(page.outputs) && page.outputs.some((x) => x.label), defect: null };
    const o = out[label];
    if (o.outputsAvailableWithoutMic && (o.micCardShown || o.microphoneRequested)) o.defect = 'microphone prompt/card although output labels were already available';
    // what the host holds for this join: the device sheet's "can't switch speakers" is
    // `supported=false` here
    o.hostAudioOutputs = await hostAudioOutputs(cl);
    if (o.hostAudioOutputs.supported === 'false') o.defect = (o.defect ? o.defect + '; ' : '') + 'host holds supported=false (the sheet would say "can\'t switch speakers")';
    if (opt.audio && j.firstPictureMs != null) {
      const k = await deviceKey(j.name, cl);
      const set = await cli(['devices', 'set', k.key, '--audio', 'on']);
      const a = await cl.secondary(); await sleep(4000); const b = await cl.secondary();
      const dc = (s) => ((s && s.webrtc && s.webrtc.dataChannels) || []).reduce((n, d) => n + (d.messagesReceived || 0), 0);
      const sync = await cl.page.ev('window.SEAudio ? SEAudio.getSyncInfo() : null').catch((e) => ({ error: e.message }));
      o.audio = { setCode: set.code, path: sync && sync.path, contextState: sync && sync.contextState, currentSink: sync && sync.currentSink, sinkTier: sync && sync.sinkTier,
        depthMs: sync && sync.depthMs, underruns: sync && sync.underruns, dataChannelMessagesIn4s: dc(b) - dc(a), trackPacketsIn4s: ((b.webrtc && b.webrtc.audioTrack && b.webrtc.audioTrack.packets) || 0) - ((a.webrtc && a.webrtc.audioTrack && a.webrtc.audioTrack.packets) || 0) };
    }
    if (opt.rejoin && grant === null && !caseOpt) {
      await leave(cl);
      const j2 = await joinClient(cl, REQ.client, { grant: null, micChoice: 'skip' });
      const p2 = await readPage(cl);
      o.rejoin = { join: summarise(j2), micCardShown: j2.modals.some((m) => /^micCard/.test(m)), microphoneRequested: (p2.gum || []).some((g) => /audio/.test(g.constraints)), outputPlan: p2.outputPlan, stored: p2.stored };
    }
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
      const ka = await deviceKey(ja.name, clA);
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
  // the tab goes away and a new one rejoins inside the grace period
  await cl.away(); const leftAt = Date.now();
  await sleep(2000);
  const j2 = await joinClient(cl, REQ.client, { name: j1.name });
  const s2 = await hostJoins(); const m2 = await ourMonitors();
  out.rejoinWithinGrace = { rejoinedAfterMs: Date.now() - leftAt, join: j2, joins: s2.joins, monitors: m2, sameDisplayId: m1.monitors && m2.monitors && m1.monitors.length === 1 && m2.monitors.length === 1 ? m1.monitors[0].id === m2.monitors[0].id : null };
  // leave and wait out the grace period: the display must go
  await cl.away(); const left2 = Date.now();
  const timeline = [];
  for (let i = 0; i < 30; i++) { await sleep(1000); const m = await ourMonitors(); timeline.push({ s: Math.round((Date.now() - left2) / 1000), displays: m.monitors ? m.monitors.length : null }); if (m.monitors && m.monitors.length === 0 && i > 2) break; }
  out.afterLeave = { timeline, displayRemoved: timeline.length ? timeline[timeline.length - 1].displays === 0 : null, removedAfterS: (timeline.find((t) => t.displays === 0) || {}).s ?? null, configuredGraceSecs: grace };
  return out;
}

async function slowAnswer(cl) {
  const n = typeof REQ.slow_answer === 'object' && REQ.slow_answer.n ? REQ.slow_answer.n : 5;
  const out = { attempts: [] };
  const name = deviceName();
  // `cpu_burn: {threads, seconds}`: saturate the CPU during the joins (a lost request or answer
  // only shows on a loaded machine). A separate process of busy workers at normal priority,
  // killed when the joins end or after `seconds`.
  let burn = null;
  if (REQ.cpu_burn) {
    const n = REQ.cpu_burn.threads || os.cpus().length, secs = REQ.cpu_burn.seconds || 120;
    const src = `const { Worker } = require('worker_threads'); for (let i = 0; i < ${n}; i++) new Worker('const e = Date.now() + ${secs * 1000}; while (Date.now() < e) {}', { eval: true }); setTimeout(() => process.exit(0), ${secs * 1000 + 500});`;
    burn = spawn(process.execPath, ['-e', src], { stdio: 'ignore', windowsHide: true });
    owned.stages.push(burn); out.cpuBurn = { threads: n, seconds: secs, pid: burn.pid };
    await sleep(1500);
  }
  for (let i = 0; i < n; i++) {
    const j = await joinClient(cl, REQ.client, { name });
    // Every attempt's full join record (network timing per /whep request included) goes into
    // joins[] too, so a slow or lost answer can be laid against serve.log afterwards.
    R.joins.push({ scenario: `slow_answer#${i + 1}`, ...j });
    const s = await hostJoins(); const m = await ourMonitors();
    const e = { i: i + 1, answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers, pageError: j.pageError, liveJoins: s.joins ? s.joins.length : null, displays: m.monitors ? m.monitors.length : null };
    // `hold_s`: stay on the stream before leaving, long enough for the page's frame watchdog
    // (5 s) to have fired, then read which video path the page ended on and how many peer
    // connections it built (a demotion or a rebuild shows up here).
    const holdS = typeof REQ.slow_answer === 'object' && REQ.slow_answer.hold_s ? REQ.slow_answer.hold_s : 0;
    if (holdS && j.firstPictureMs != null) {
      await sleep(holdS * 1000);
      e.afterHold = await cl.page.ev('({ videoPath: window.SEVideo ? SEVideo.getStats().videoPath : null, pcs: window.__se ? __se.pcs.length : null, isLive: typeof isLive !== "undefined" ? isLive : null })').catch((x) => ({ error: x.message }));
      e.demotedLines = cl.console.filter((c) => /no WebCodecs frame|fallbackToVideo|falling back to <video>|demot/i.test(c.text)).length;
    }
    e.flag = (j.answerMs > 5000 ? 'answer over 5 s; ' : '') + (/did not answer|gave up|timed out/i.test(j.pageError || '') ? 'page gave up; ' : '') + ((e.liveJoins || 0) > 1 || (e.displays || 0) > 1 ? 'second session/display; ' : '');
    out.attempts.push(e);
    await leave(cl); await sleep(2500);
  }
  if (burn && burn.exitCode === null) { try { burn.kill(); } catch { /* gone */ } }
  out.flagged = out.attempts.filter((a) => a.flag).length;
  return out;
}

// The cursor stand-in at rest. A small window moves (fast steps, like a pointer) and stops; the
// stage reports where. Does the client's picture come to rest there, or one step short? A frame
// gate that drops the frame carrying the final position leaves the picture stale until
// something else changes (on a still desktop, indefinitely).
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
  const stop = await stopHostGated(H, { exe: HOSTCTX.exe, env: cliEnv(), useStopVerb: !!R.host.controlVerbs });
  out.stop = stop;
  const downAt = Date.now();
  await sleep(3000);
  await displayGate('host restart; the open page rejoins on its own');
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

// A join that fails must give back everything it armed. Each case makes a join fail
// without breaking the driver, then reads the page's own state (no event is dispatched; nothing
// reaches the host's injector) and watches the network for requests the page keeps sending.
const JOIN_FAIL_PROBE = `(() => {
  const q = (id) => document.getElementById(id);
  const vis = (el) => { if (!el) return null; const s = getComputedStyle(el); return s.display !== 'none' && s.visibility !== 'hidden' && el.offsetWidth > 0; };
  const st = (window.SEVideo && SEVideo.getStats) ? SEVideo.getStats() : {};
  const v = (f) => { try { return f(); } catch (e) { return 'n/a'; } };
  return {
    joinFormVisible: vis(q('join')), connectDisabled: q('connect') ? q('connect').disabled : null,
    error: (q('errorMessage') || {}).textContent || '', loadingShown: q('loading') ? q('loading').classList.contains('show') : null,
    stageVisible: vis(q('stage')),
    locked: v(() => locked), isLive: v(() => isLive), streamInputArmed: v(() => streamInputArmed),
    inputSuspended: window.RemoteInput && RemoteInput.isSuspended ? RemoteInput.isSuspended() : null,
    beforeUnloadHandler: v(() => !!beforeUnloadHandler), visibilityHandler: v(() => !!visibilityHandler), fullscreenHandler: v(() => !!fullscreenHandler),
    wakeLockHeld: st.wakeLock, wakeLockSentinel: v(() => (wakeLockSentinel ? (wakeLockSentinel.released ? 'released' : 'held') : 'none')),
    noSleepEnabled: v(() => !!(noSleepImpl && noSleepImpl.isEnabled)),
    fullscreen: st.fullscreen, pointerLock: !!document.pointerLockElement,
    activeElement: document.activeElement ? (document.activeElement.id || document.activeElement.tagName) : null,
    audioContext: window.SEAudio && SEAudio.getSyncInfo ? (SEAudio.getSyncInfo() || {}).contextState : null,
  }; })()`;
async function joinFailures() {
  const o = typeof REQ.join_failures === 'object' && !Array.isArray(REQ.join_failures) ? REQ.join_failures : {};
  const want = Array.isArray(REQ.join_failures) ? REQ.join_failures : (o.cases || ['wrong_otp', 'unknown_session', 'host_stopped']);
  const out = {};
  for (const key of want) {
    const cl = await openClient(REQ.client, { offset: 3 });
    const name = deviceName();
    let { origin, url } = clientUrl(REQ.client, name);
    if (key === 'wrong_otp') url = url.replace(/otp=\d+/, 'otp=' + String((Number(HOSTCTX.otp) + 1) % 1000000).padStart(6, '0'));
    if (key === 'unknown_session') url = url.replace(/id=[^&]+/, 'id=' + 'ABCDEFGHJKMN'.split('').reverse().join(''));
    const r = { case: key };
    try {
      await underLock('join-failure:' + key, async () => {
        await cl.page.send('Page.navigate', { url });
        for (let i = 0; i < 75; i++) { if (await cl.page.ev('document.readyState === "complete" && !!document.getElementById("joinForm")').catch(() => false)) break; await sleep(200); }
        await cl.chrome.browser.send('Browser.grantPermissions', { origin, permissions: STD_GRANT }).catch(() => {});
        r.modals = [];
        for (let i = 0; i < 10; i++) { const m = await cl.sweepModals(); if (m) r.modals.push(m); await sleep(150); }
        r.before = await cl.page.ev(JOIN_FAIL_PROBE).catch((e) => ({ error: e.message }));
        const submitAt = await cl.page.ev('(() => { const t = performance.timeOrigin + performance.now(); document.getElementById("joinForm").requestSubmit(); return t; })()', { gesture: true });
        const t0 = Date.now();
        if (key === 'host_stopped') {
          // stop the host once the offer is on the wire (the host is then creating the display)
          for (let i = 0; i < 100 && !cl.whep.some((w) => w.method === 'POST' && w.sent >= submitAt - 50); i++) await sleep(50);
          await sleep(o.stop_after_ms || 600);
          r.stoppedAfterMs = Date.now() - t0;
          r.stop = await stopHostGated(H, { exe: HOSTCTX.exe, env: cliEnv(), useStopVerb: !!R.host.controlVerbs });
        }
        // wait for the page to settle on a failure: Connect enabled again, or 45 s
        let settled = null;
        for (let i = 0; i < 225; i++) {
          const s = await cl.page.ev(JOIN_FAIL_PROBE).catch(() => null);
          r.during = r.during || [];
          if (s && (i % 10 === 0)) r.during.push({ ms: Date.now() - t0, connectDisabled: s.connectDisabled, error: s.error, locked: s.locked, isLive: s.isLive });
          if (s && s.connectDisabled === false && !s.isLive) { settled = Date.now() - t0; break; }
          await sleep(200);
        }
        r.settledMs = settled;
        const failAt = performance.timeOrigin + performance.now();
        await sleep(o.watch_ms || 8000);
        r.after = await cl.page.ev(JOIN_FAIL_PROBE).catch((e) => ({ error: e.message }));
        const sec = await cl.secondary();
        r.peerConnections = sec.peerConnections; r.openPeerConnections = sec.openPeerConnections;
        r.whep = cl.whep.filter((w) => w.sent >= submitAt - 50).map((w) => ({ method: w.method, status: w.status, failed: w.failed, sentMs: Math.round(w.sent - submitAt), answeredMs: w.answered ? Math.round(w.answered - submitAt) : null }));
        r.offers = r.whep.filter((w) => w.method === 'POST').length;
        const settledAbs = settled != null ? submitAt + settled : failAt;
        r.requestsAfterFailure = [...cl.api, ...cl.whep].filter((w) => w.sent > settledAbs + 300).map((w) => ({ method: w.method, url: w.url, sentMs: Math.round(w.sent - settledAbs) }));
        r.console = cl.console.filter((c) => c.t >= t0 - 1000).slice(0, 20);
        if (key === 'host_stopped') { await sleep(2000); await displayGate('host restart; a waiting page may rejoin'); H = await bootHost('restart-after-join-failure'); }
        const a = r.after || {};
        r.pass = {
          // #stage is the page's container and is laid out before a join too, so "back" means
          // the form is shown and the stage is as it was before Connect.
          formBack: a.joinFormVisible === true && a.stageVisible === (r.before || {}).stageVisible,
          connectEnabled: a.connectDisabled === false,
          wakeLockReleased: a.wakeLockHeld === false && a.noSleepEnabled !== true,
          noLeavePrompt: a.beforeUnloadHandler === false,
          inputNotCapturing: a.streamInputArmed === false && a.locked === false,
          noOpenPeerConnection: (r.openPeerConnections || 0) === 0,
          noRequestsAfterFailure: r.requestsAfterFailure.length === 0,
          errorShown: !!(a.error && a.error.trim()),
        };
        r.ok = Object.values(r.pass).every(Boolean);
      });
      const m = await ourMonitors(); r.displaysAfter = m.monitors ? m.monitors.length : null;
    } catch (e) { r.error = e.stack || e.message; }
    finally { await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1); }
    out[key] = r;
  }
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
  // `fallback_paths` as a list names the tiers to run (by label); `true` runs them all
  for (const [label, o] of tiers.filter(([l]) => !Array.isArray(REQ.fallback_paths) || REQ.fallback_paths.includes(l))) {
    const spec = { ...REQ.client, https: o.https !== false };
    const cl = await openClient(spec, { offset: 2, flags: o.flags || [], disable: o.disable || [] });
    try {
      const j = await joinClient(cl, spec);
      if (j.firstPictureMs == null) { out[label] = { error: 'no picture', join: j }; continue; }
      const w = await timedWindow(cl, j.name, { seconds: Math.min(REQ.duration, 10), mover: true, large: false, label: 'fallback:' + label });
      const sec = await cl.secondary();
      out[label] = { join: { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, secure: j.page && j.page.secure, coi: j.page && j.page.coi }, latencyMs: w.latencyMs, paintedFps: w.paintedFps, longestGapMs: w.longestGapMs, decodeErrors: w.decodeErrors, loaded: w.loaded, pageCounters: w.pageCounters, videoPath: sec.seVideo && sec.seVideo.videoPath, renderTier: sec.seVideo && sec.seVideo.renderTier, transportTier: sec.seVideo && sec.seVideo.transportTier, decoder: sec.webrtc && sec.webrtc.video ? sec.webrtc.video.decoder : null, usingFallback: sec.usingFallback, error: w.error };
      await leave(cl);
    } finally { await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1); }
  }
  return out;
}

// One fresh browser per client audio tier, each running the full audio test. The standard Opus
// track is tapped where it reaches an <audio> element, so the track tiers are measured the same
// way as the worklet ones. `stale_output` saves an output id this client does not have (what a
// device row keeps after its hardware went away) and replays it into the running host first.
const AUDIO_TIERS = {
  default: { label: 'default (isolated page, SharedArrayBuffer ring)' },
  no_sab: { label: 'no SharedArrayBuffer (private ring)', disable: ['sab'] },
  no_audio_decoder: { label: 'no AudioDecoder (standard Opus track from the start)', disable: ['audio_decoder'] },
  plain_http: { label: 'plain HTTP (no secure context)', https: false },
  worker_fails: { label: 'decode worker fails to load after the fast path was claimed', disable: ['audio_worker'] },
  stale_output: { label: 'saved output id that this client does not have', staleSink: 'se-handler-gone-output' },
};
async function audioPaths() {
  const want = Array.isArray(REQ.audio_paths) ? REQ.audio_paths : Object.keys(AUDIO_TIERS);
  const out = {};
  for (const key of want) {
    const t = AUDIO_TIERS[key];
    if (!t) { out[key] = { error: 'unknown audio tier; known: ' + Object.keys(AUDIO_TIERS).join(', ') }; continue; }
    const spec = { ...REQ.client, https: t.https !== false };
    const cl = await openClient(spec, { offset: 2, disable: t.disable || [] });
    try {
      const j = await joinClient(cl, spec);
      if (j.firstPictureMs == null) { out[key] = { label: t.label, error: 'no picture', join: j }; await leave(cl); continue; }
      const beforeClicks = t.staleSink ? async (k) => {
        const g = await cli(['config', 'get', 'devices']);
        let rows = null; try { rows = JSON.parse(g.stdout.slice(g.stdout.indexOf('['))); } catch { /* none */ }
        const i = rows ? rows.findIndex((r) => r.token === k.key || r.ip === k.key) : -1;
        if (i < 0) return { error: 'no saved row for this device' };
        const r = await cli(['config', 'set', `devices.${i}.audioOutputDeviceId`, t.staleSink]);
        await sleep(3500); // a few /reconfig polls carry the new sink
        const page = await cl.page.ev('(window.SEAudio && SEAudio.getSyncInfo) ? SEAudio.getSyncInfo() : null').catch((e) => ({ error: e.message }));
        return { set: { code: r.code, out: (r.stdout + r.stderr).trim().slice(0, 200) }, syncInfoAfter: page };
      } : null;
      const a = await audioTest(cl, j.name, { beforeClicks });
      out[key] = { label: t.label, join: { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers, modals: j.modals }, ...a };
      await leave(cl);
    } finally { await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1); }
  }
  return out;
}

// receive_hold: Chromium releases received video frames to an encoded transform on a 64 Hz
// metronome (rtc_encoded_video_stream_transformer.cc), so the read instants are compared against
// 15.625 ms. One join per browser; each rate is an in-place renegotiation (one display mode change).
function timerResolution() {
  // NtQueryTimerResolution: the system timer's current period. At the default 15.625 ms every
  // timed wait on the machine is quantised to the same 64 Hz grid the metronome would produce,
  // so a 15.625 ms comb is only evidence of the metronome when the resolution is finer.
  const src = [
    'Add-Type -TypeDefinition @"',
    'using System; using System.Runtime.InteropServices;',
    'public static class SeTimer { [DllImport("ntdll.dll")] public static extern int NtQueryTimerResolution(out uint min, out uint max, out uint cur); }',
    '"@',
    "$a=0;$b=0;$c=0; [void][SeTimer]::NtQueryTimerResolution([ref]$a,[ref]$b,[ref]$c); '{0} {1} {2}' -f $a,$b,$c",
  ].join('\n');
  try {
    const [mx, mn, cur] = execFileSync('powershell.exe', ['-NoProfile', '-Command', src], { encoding: 'utf8', windowsHide: true, timeout: 20000 }).trim().split(/\s+/).map(Number);
    return { currentMs: cur / 10000, coarsestMs: mx / 10000, finestMs: mn / 10000 };
  } catch (e) { return { error: e.message }; }
}

async function workerEval(cl, expr) {
  const out = [];
  for (const sid of [...cl.workers.keys()]) {
    const r = await cl.page.send('Runtime.evaluate', { expression: expr, returnByValue: true }, sid).catch(() => null);
    out.push({ sid, value: r && r.result ? r.result.value : null });
  }
  return out;
}

const r2 = (x) => (x == null ? null : Math.round(x * 100) / 100);
function dumpRows(file, data) {
  if (!RUN_DIR) return null;
  const p = path.join(RUN_DIR, file);
  try { fs.writeFileSync(p, JSON.stringify(data)); return p; } catch { return null; }
}

async function rxWindow(cl, name, { hz, seconds, large = true, relay = null, label = null }) {
  const lbl = label || `receive_hold@${hz}`;
  const gate = await loadGate(lbl);
  const st = await stageFor(name, { seconds: seconds + REQ.warmup + 6, stamp: true });
  const ready = await st.ready;
  if (ready.error) { stopStage(st); return { error: 'stage: ' + ready.error }; }
  const lg = large ? startStage(ready.gdi, { seconds: seconds + REQ.warmup + 6, stamp: false, large: true }) : null;
  if (lg) { const lr = await lg.ready; if (lr.error) { stopStage(st); stopStage(lg); return { error: 'large-area stage: ' + lr.error }; } }
  try {
    return await underLock(lbl, async () => {
      await sleep(REQ.warmup * 1000);
      const load = startLoadSampler();
      const a = await cl.secondary();
      const armed = await workerEval(cl, RX_ARM);
      if (relay) await relay.armFrames();
      await sleep(seconds * 1000);
      const reads = await workerEval(cl, RX_READ);
      const relayFrames = relay ? await relay.readFrames() : null;
      const b = await cl.secondary(); const loadSum = load.stop();
      const best = reads.filter((x) => x.value && x.value.n).sort((x, y) => y.value.n - x.value.n)[0];
      if (!best) return { error: 'no worker recorded a frame', armed: armed.map((x) => x.value), ...loadedVerdict(gate, loadSum) };
      // onto the precise system clock: this worker's own offset (Client.contextOffset)
      const off = await cl.contextOffset(best.sid);
      const rows = rxRows(best.value, off ? -off.off : 0);
      const analysis = analyzeRx(rows, { sourceHz: hz, relay: relayFrames });
      return { hz, ...loadedVerdict(gate, loadSum), load: loadSum, workerClockMinusSystemMs: off ? r2(off.off) : null, workerOffsetRttMs: off ? r2(off.rtt) : null,
        metadataFields: best.value.fields, metadataSample: best.value.sample, hookError: best.value.err, dropped: best.value.dropped, ...analysis,
        pageCounters: pageDelta(a, b, seconds), rawFile: dumpRows(`rx-${cl.browser}-${hz}-${Date.now()}.json`, { rows, relay: relayFrames }) };
    });
  } finally { await st.exited.catch(() => {}); stopStage(st); if (lg) { await lg.exited.catch(() => {}); stopStage(lg); } }
}


// A join that never painted (the page's offer went unanswered and its own retry did not land
// either) is retried once from a fresh page, so one lost answer does not void a whole request.
async function joinRetry(cl, spec, opts) {
  const j = await joinClient(cl, spec, opts);
  if (j.firstPictureMs != null) return j;
  log(`join ${j.name} painted nothing: recording why, then joining once more`);
  const nc = await neverConnected(cl, j);
  R.neverConnected = (R.neverConnected || []).concat([nc]);
  const j2 = await joinClient(cl, spec, opts);
  j2.firstAttempt = { answerMs: j.answerMs, offers: j.offers, pageError: j.pageError, whepRequests: j.whepRequests, neverConnected: nc };
  return j2;
}

// The page's side of a join that never painted (ICE state, candidates gathered and sent, every
// PATCH and its answer), then how long the host keeps the display once the page is gone.
async function neverConnected(cl, j) {
  const ice = await cl.page.ev(`(async () => { const out = []; for (const pc of (window.__se && __se.pcs) || []) { const o = { connection: pc.connectionState, ice: pc.iceConnectionState, gathering: pc.iceGatheringState, signaling: pc.signalingState, local: [], remote: [], pairs: [] }; try { const st = await pc.getStats(); st.forEach((x) => { if (x.type === 'local-candidate') o.local.push(x.candidateType + ' ' + (x.protocol || '') + ' ' + (x.address || x.ip || '') + ':' + x.port); if (x.type === 'remote-candidate') o.remote.push(x.candidateType + ' ' + (x.address || x.ip || '') + ':' + x.port); if (x.type === 'candidate-pair') o.pairs.push(x.state + (x.nominated ? ' nominated' : '')); }); } catch (e) { o.error = String(e); } out.push(o); } return out; })()`).catch((e) => [{ error: e.message }]);
  const patches = cl.whep.filter((w) => w.method === 'PATCH' && w.sent >= (j.submitAt || 0) - 50).map((w) => ({ sentMs: Math.round(w.sent - j.submitAt), status: w.status || null, answeredMs: w.answered ? Math.round(w.answered - j.submitAt) : null, failed: w.failed || null }));
  const host = { joins: (await hostJoins()).joins, monitors: await ourMonitors() };
  await cl.away();
  const goneAt = Date.now(); const tl = [];
  for (let i = 0; i < 60; i++) { await sleep(1000); const m = await ourMonitors(); const n = m.monitors ? m.monitors.length : null; tl.push(n); if (n === 0) break; }
  return { name: j.name, answerMs: j.answerMs, offers: j.offers, pageError: j.pageError, ice, patches, hostAtFailure: host, displayKeptAfterPageGoneS: tl[tl.length - 1] === 0 ? Math.round((Date.now() - goneAt) / 1000) : null, displaysTimeline: tl };
}

async function attachRelay(cl, cfg = {}) {
  const r = new Relay({ hostIp: H.ip, cfg });
  owned.relays.push(r);
  await r.attach(cl.page);
  const stop0 = r.stop.bind(r);
  r.stop = async () => { const i = owned.relays.indexOf(r); if (i >= 0) owned.relays.splice(i, 1); return stop0(); };
  return r;
}

/** Is the selected pair the relay (remote port is a relay port, and the relay counted media)? */
async function relayVerdict(cl, relay) {
  const pair = await cl.page.ev(`(async () => { const out = []; for (const pc of (window.__se && __se.pcs) || []) { try { const st = await pc.getStats(); let sel = null; st.forEach((x) => { if (x.type === 'transport' && x.selectedCandidatePairId) sel = st.get(x.selectedCandidatePairId); }); if (!sel) st.forEach((x) => { if (x.type === 'candidate-pair' && x.nominated && x.state === 'succeeded') sel = x; }); if (sel) { const rc = st.get(sel.remoteCandidateId), lc = st.get(sel.localCandidateId); out.push({ remote: rc && (rc.address || rc.ip) + ':' + rc.port, remoteType: rc && rc.candidateType, local: lc && (lc.address || lc.ip) + ':' + lc.port, state: pc.connectionState }); } } catch (e) { out.push({ error: String(e) }); } } return out; })()`).catch((e) => [{ error: e.message }]);
  const st = await relay.stats(0);
  const live = pair.filter((p) => p.state === 'connected');
  const ports = new Set(Object.values((st && st.ports) || {}).map(String));
  const viaRelay = live.length > 0 && live.every((p) => ports.has(String((p.remote || '').split(':').pop())));
  return { selectedPairs: pair, relayDownPackets: st ? st.totals.down.in : null, relayUpPackets: st ? st.totals.up.in : null, viaRelay: viaRelay && !!(st && st.totals.down.in > 0) };
}

// impair: one join whose media goes through lib/relay.mjs; per scenario one continuous stamp
// window in three phases: baseline (clean), impaired, recovery (clean again). Host telemetry is
// sampled once a second throughout (`status --json`), the relay reports per second.
const IMPAIRMENTS = {
  stall200: { label: 'periodic 200 ms stall every 3 s (host -> browser held, released in order)', down: { stall: { ms: 200, every_s: 3 } } },
  loss1: { label: '1 % random loss, host -> browser', down: { loss: 0.01 } },
  loss5: { label: '5 % random loss, host -> browser', down: { loss: 0.05 } },
  loss20: { label: '20 % random loss, host -> browser, no cap', down: { loss: 0.2 } },
  loss20_long: { label: '20 % random loss held for 120 s, host -> browser, no cap', down: { loss: 0.2 }, impaired_s: 120 },
  burst: { label: 'burst loss (Gilbert-Elliott: enter 0.5 %, leave 30 %, all lost in a burst), host -> browser', down: { burst: { p_enter: 0.005, p_exit: 0.3, loss: 1 } } },
  loss5_up: { label: '5 % random loss, browser -> host (feedback, NACK, PLI, input)', up: { loss: 0.05 } },
  delay_jitter: { label: '30 ms delay with +-10 ms uniform jitter, order kept, both directions', down: { delay_ms: 30, jitter_ms: 10 }, up: { delay_ms: 30, jitter_ms: 10 } },
  delay_jitter_long: { label: '30 ms delay with +-10 ms uniform jitter held for 90 s, order kept, both directions', down: { delay_ms: 30, jitter_ms: 10 }, up: { delay_ms: 30, jitter_ms: 10 }, impaired_s: 90 },
  delay_jitter30: { label: '40 ms delay with +-30 ms uniform jitter, order kept, both directions', down: { delay_ms: 40, jitter_ms: 30 }, up: { delay_ms: 40, jitter_ms: 30 } },
  cap5: { label: '5 Mbit/s cap, host -> browser (token bucket, 100 ms drop-tail queue)', down: { rate_kbps: 5000 } },
  cap1_5: { label: '1.5 Mbit/s cap, host -> browser (token bucket, 100 ms drop-tail queue)', down: { rate_kbps: 1500 } },
  cap_change: { label: 'cap that changes mid-run: 8 -> 1.5 -> 8 Mbit/s, host -> browser', down: { rate_kbps: 8000 }, schedule: [{ at_s: 7, down: { rate_kbps: 1500 } }, { at_s: 14, down: { rate_kbps: 8000 } }] },
  // a base delay that moves is not congestion: nothing should be cut for it
  delay_step: { label: 'base delay step 5 -> 60 -> 5 ms (at 7 s and 14 s), both directions, order kept, no loss', down: { delay_ms: 5 }, up: { delay_ms: 5 }, schedule: [{ at_s: 7, down: { delay_ms: 60 }, up: { delay_ms: 60 } }, { at_s: 14, down: { delay_ms: 5 }, up: { delay_ms: 5 } }] },
  delay_step_loss1: { label: 'base delay step 5 -> 60 -> 5 ms (at 7 s and 14 s) with 1 % random loss throughout, order kept', down: { delay_ms: 5, loss: 0.01 }, up: { delay_ms: 5 }, schedule: [{ at_s: 7, down: { delay_ms: 60, loss: 0.01 }, up: { delay_ms: 60 } }, { at_s: 14, down: { delay_ms: 5, loss: 0.01 }, up: { delay_ms: 5 } }] },
  loss_step3: { label: 'loss step: none, 3 % random loss from 5 s to 12 s, none, host -> browser', down: {}, schedule: [{ at_s: 5, down: { loss: 0.03 } }, { at_s: 12, down: {} }] },
  cap3: { label: '3 Mbit/s cap, host -> browser (token bucket, 100 ms drop-tail queue)', down: { rate_kbps: 3000 } },
  cap2: { label: '2 Mbit/s cap, host -> browser (token bucket, 100 ms drop-tail queue)', down: { rate_kbps: 2000 } },
  cap_slowfill: { label: 'cap at 98 % of the input rate measured in this baseline, host -> browser (token bucket, 500 ms drop-tail queue)', down: { rate_of_baseline: 0.98, queue_ms: 500 } },
  cap5_long: { label: '5 Mbit/s cap held for 60 s, host -> browser (token bucket, 100 ms drop-tail queue)', down: { rate_kbps: 5000 }, impaired_s: 60 },
};
const DEFAULT_IMPAIRMENTS = ['stall200', 'loss1', 'loss5', 'cap5', 'cap1_5', 'cap_change'];

/** `status --json` once a period, reduced to the counters an impairment moves. */
function hostSampler(periodMs = 1000) {
  const samples = []; let on = true;
  const pickSession = (tel) => {
    const s = tel && tel.sessions ? Object.values(tel.sessions) : [];
    return s.sort((a, b) => (b.frames_encoded || 0) - (a.frames_encoded || 0))[0] || null;
  };
  const loop = (async () => {
    while (on) {
      const t0 = Date.now();
      const r = await cli(['status', '--json'], 8000); const j = cliJson(r);
      const s = pickSession(j && j.telemetry);
      if (s) {
        const d = s.degradation || {};
        const lg = s.link_governor || null;
        samples.push({ t: t0, ms: r.ms, frames: s.frames_encoded, bytes: s.bytes_sent, encoder: s.encoder, qp: s.qp_governor ? s.qp_governor.qp_effective : (lg && lg.qp_effective != null ? lg.qp_effective : null), ceilingMbps: s.qp_governor ? s.qp_governor.ceiling_mbps : null,
          kfReq: d.keyframe_requests, kfSuppressed: d.keyframe_requests_suppressed, clientKfReq: d.client_keyframe_requests, idrs: d.idrs_sent, nacks: d.nacks_received, bitrateCuts: d.bitrate_cuts, overuse: d.delay_overuse_events, pacerMiss: d.pacer_deadline_misses,
          clientPainted: s.client ? s.client.frames_painted : null, clientDropped: s.client ? s.client.frames_dropped : null,
          packets: s.packets_sent ?? null, fps: s.fps ?? null, client: s.client || null,
          captureBackend: s.capture_backend || null, link: lg, rtx: rtxCounters(s) });
      } else samples.push({ t: t0, ms: r.ms, error: 'no session in status --json' });
      const wait = periodMs - (Date.now() - t0); if (wait > 0) await sleep(wait);
    }
  })();
  return { async stop() { on = false; await loop; return samples; } };
}
// numeric leaves of any retransmission object the host reports, flattened by path, so new
// counters reach the timeline without a change here
const RTX_KEY = /retrans|rtx|resen[dt]/i;
function rtxCounters(s) {
  const out = {};
  const walk = (v, p) => {
    if (typeof v === 'number' && Number.isFinite(v)) out[p] = v;
    else if (v && typeof v === 'object' && !Array.isArray(v)) for (const k of Object.keys(v)) walk(v[k], p + '.' + k);
  };
  for (const [where, o] of [['', s], ['degradation', s.degradation], ['link_governor', s.link_governor]]) {
    if (!o || typeof o !== 'object') continue;
    for (const k of Object.keys(o)) if (RTX_KEY.test(k)) walk(o[k], where ? where + '.' + k : k);
  }
  return Object.keys(out).length ? out : null;
}

/**
 * One row per host sample interval: the host's encoder output and counters, the relay's down
 * direction over the same interval, what the client painted and the governor's state. Relay rows
 * and host samples share the system clock; stamps are moved onto it with `offsetMs`.
 */
function impairTimeline(hostSamples, relaySeconds, stampSamples, { t0, tB, tC, offsetMs }) {
  const hs = hostSamples.filter((x) => !x.error);
  const first = new Map();
  for (const x of stampSamples || []) {
    if (x.stamp === undefined) continue;
    const t = x.ts - offsetMs;
    // the same plausibility window stampMetrics applies
    if (!(t - x.stamp > -5 && t - x.stamp < 5000)) continue;
    if (!first.has(x.stamp) || first.get(x.stamp) > t) first.set(x.stamp, t);
  }
  const seen = [...first.entries()].map(([st, t]) => ({ t, lat: t - st })).sort((x, y) => x.t - y.t);
  const r2 = (v) => (v == null || !Number.isFinite(v) ? null : Math.round(v * 100) / 100);
  const rows = [];
  for (let i = 1; i < hs.length; i++) {
    const a = hs[i - 1], b = hs[i]; const dt = (b.t - a.t) / 1000;
    if (!(dt > 0)) continue;
    const reset = a.frames != null && b.frames != null && b.frames < a.frames;
    const d = (k) => (reset || a[k] == null || b[k] == null ? null : b[k] - a[k]);
    const rate = (k, f = 1) => { const v = d(k); return v == null ? null : r2(v * f / dt); };
    const row = { s: Math.round((b.t - t0) / 100) / 10, phase: b.t <= tB ? 'baseline' : b.t <= tC ? 'impaired' : 'recovery', dt: r2(dt),
      hostMbps: rate('bytes', 8 / 1e6), fps: rate('frames'), idrs: d('idrs'), nacks: d('nacks'), kfReq: d('kfReq'), bitrateCuts: d('bitrateCuts'), overuse: d('overuse') };
    if (reset) row.sessionChanged = true;
    const rr = (relaySeconds || []).filter((r) => r.t > a.t && r.t <= b.t);
    const sum = (dir, k) => rr.reduce((x, r) => x + ((r[dir] && r[dir][k]) || 0), 0);
    row.relayInMbps = rr.length ? r2(sum('down', 'inBytes') * 8 / 1e6 / dt) : null;
    row.relayOutMbps = rr.length ? r2(sum('down', 'sentBytes') * 8 / 1e6 / dt) : null;
    row.queueDrops = rr.length ? sum('down', 'queueDrops') : null;
    row.lossDrops = rr.length ? sum('down', 'lossDrops') : null;
    row.queueMs = rr.some((r) => r.down.queueMs != null) ? Math.max(...rr.map((r) => r.down.queueMs || 0)) : null;
    row.queueBytes = rr.length ? Math.max(...rr.map((r) => r.down.queueBytes || 0)) : null;
    row.upLossDrops = rr.length ? sum('up', 'lossDrops') : null;
    row.clientPainted = rate('clientPainted');
    const st = seen.filter((x) => x.t > a.t && x.t <= b.t).map((x) => x.lat).sort((x, y) => x - y);
    row.stampsPainted = r2(st.length / dt);
    row.latencyP50 = st.length ? r2(st[st.length >> 1]) : null;
    const g = b.link, ga = a.link;
    if (g) {
      const dg = (k) => (ga && typeof g[k] === 'number' && typeof ga[k] === 'number' ? g[k] - ga[k] : null);
      const ev = g.evidence || {};
      row.gov = { state: g.state ?? null, verdict: g.verdict ?? null, qp: g.qp_effective ?? null, qpLink: g.qp_link ?? null, frameDivisor: g.frame_divisor ?? null, clientFrameDivisor: g.client_frame_divisor ?? null,
        steps: dg('steps'), raises: dg('raises'), clientCuts: dg('client_cuts'), queueMs: ev.queue_ms ?? null, deliveredMbps: ev.delivered_mbps ?? null, sentMbps: ev.sent_mbps ?? null, loss: ev.loss ?? null };
      if (g.last_move && (!ga || ga.last_move !== g.last_move)) row.gov.lastMove = g.last_move;
    } else row.gov = null;
    if (b.rtx) {
      const o = {};
      for (const k of Object.keys(b.rtx)) o[k] = !reset && a.rtx && a.rtx[k] != null ? b.rtx[k] - a.rtx[k] : null;
      row.rtx = o;
    }
    rows.push(row);
  }
  return rows;
}

/** The timeline as fixed-width text, one line per row; '*' marks the impaired phase. */
function timelineTable(rows) {
  const f = (v, w, p = 2) => (v == null ? '-' : typeof v === 'number' ? v.toFixed(p) : String(v)).padStart(w);
  const out = ['     s | host Mbit   fps  idr  nack cuts | relay in   out  qdrop  q ms | painted stamps   p50 | gov qp div state'];
  for (const r of rows) {
    const g = r.gov;
    const gov = g ? `${f(g.qp, 6, 0)} ${f(g.frameDivisor, 3, 0)} ${g.state || '-'}` : '-';
    out.push(`${r.phase === 'impaired' ? '*' : ' '}${f(r.s, 5, 1)} |${f(r.hostMbps, 9)} ${f(r.fps, 5, 1)} ${f(r.idrs, 4, 0)} ${f(r.nacks, 5, 0)} ${f(r.bitrateCuts, 4, 0)} |${f(r.relayInMbps, 9)} ${f(r.relayOutMbps, 5)} ${f(r.queueDrops, 6, 0)} ${f(r.queueMs, 5, 0)} |${f(r.clientPainted, 8, 1)} ${f(r.stampsPainted, 6, 1)} ${f(r.latencyP50, 5, 0)} | ${gov}${r.sessionChanged ? ' (new session)' : ''}`);
  }
  return out;
}

/** Per phase: how much of what reached the relay was not encoder output (retransmissions, packet overhead, RTCP). */
function retransmitLoad(rows) {
  const out = {};
  for (const ph of ['baseline', 'impaired', 'recovery']) {
    const rs = rows.filter((r) => r.phase === ph && r.hostMbps != null && r.relayInMbps != null);
    const enc = rs.reduce((x, r) => x + r.hostMbps * r.dt, 0), rin = rs.reduce((x, r) => x + r.relayInMbps * r.dt, 0);
    const rtx = {};
    for (const r of rows.filter((x) => x.phase === ph && x.rtx)) for (const k of Object.keys(r.rtx)) if (r.rtx[k] != null) rtx[k] = (rtx[k] || 0) + r.rtx[k];
    out[ph] = { rows: rs.length, encoderMbit: Math.round(enc * 100) / 100, relayInMbit: Math.round(rin * 100) / 100, nonEncoderShare: rin > 0 ? Math.round((rin - enc) / rin * 1000) / 1000 : null, ...(Object.keys(rtx).length ? { rtx } : {}) };
  }
  const b = out.baseline.nonEncoderShare, i = out.impaired.nonEncoderShare;
  out.impairedExcessShare = b != null && i != null ? Math.round((i - b) * 1000) / 1000 : null;
  out.note = 'nonEncoderShare = (relay input - encoder output) / relay input; the baseline share is packet overhead, the impaired share above it is mostly retransmission';
  return out;
}

/** Host counters across [from, to] (ms, node clock): deltas and the bitrate series. */
function hostPhase(samples, from, to) {
  const inn = samples.filter((s) => s.t >= from - 600 && s.t <= to + 600 && !s.error);
  if (inn.length < 2) return { samples: inn.length };
  const a = inn[0], b = inn[inn.length - 1]; const secs = (b.t - a.t) / 1000;
  // a renegotiation (a demoted video path) is a new session whose counters start again
  if (inn.some((s, i) => i > 0 && s.frames != null && inn[i - 1].frames != null && s.frames < inn[i - 1].frames)) return { samples: inn.length, sessionChanged: true, mbpsSeries: [] };
  const dl = (k) => (a[k] != null && b[k] != null ? b[k] - a[k] : null);
  const mbps = []; for (let i = 1; i < inn.length; i++) { const dt = (inn[i].t - inn[i - 1].t) / 1000; if (dt > 0 && inn[i].bytes != null) mbps.push(+((inn[i].bytes - inn[i - 1].bytes) * 8 / dt / 1e6).toFixed(2)); }
  return { samples: inn.length, seconds: +secs.toFixed(1), meanMbps: secs > 0 && dl('bytes') != null ? +(dl('bytes') * 8 / secs / 1e6).toFixed(2) : null, mbpsSeries: mbps, encodedFps: secs > 0 && dl('frames') != null ? +(dl('frames') / secs).toFixed(1) : null,
    qpSeries: inn.map((s) => s.qp), ceilingMbps: inn.map((s) => s.ceilingMbps), frameDivisorSeries: inn.map((s) => (s.link && s.link.frame_divisor != null ? s.link.frame_divisor : null)), keyframeRequests: dl('kfReq'), keyframeRequestsSuppressed: dl('kfSuppressed'), clientKeyframeRequests: dl('clientKfReq'), idrsSent: dl('idrs'), nacksReceived: dl('nacks'), bitrateCuts: dl('bitrateCuts'), delayOveruse: dl('overuse'), pacerDeadlineMisses: dl('pacerMiss') };
}
/** From the impairment's end: when were 4 consecutive 250 ms bins healthy again? */
function recovery(samples, { endAt, until, baseFps, baseP50, offsetMs }) {
  const ok = samples.filter((s) => s.stamp !== undefined && s.ts - offsetMs >= endAt && s.ts - offsetMs <= until).sort((x, y) => x.ts - y.ts);
  const bins = []; const n = Math.ceil((until - endAt) / 250);
  for (let i = 0; i < n; i++) bins.push({ stamps: new Set(), lat: [] });
  for (const s of ok) { const i = Math.floor((s.ts - offsetMs - endAt) / 250); if (i >= 0 && i < n && !bins[i].stamps.has(s.stamp)) { bins[i].stamps.add(s.stamp); bins[i].lat.push(s.ts - offsetMs - s.stamp); } }
  const healthy = bins.map((b) => { const fps = b.stamps.size * 4; const l = b.lat.sort((x, y) => x - y); const p50 = l.length ? l[l.length >> 1] : Infinity; return fps >= 0.8 * baseFps && p50 <= baseP50 + Math.max(10, 0.3 * baseP50); });
  for (let i = 0; i + 3 < healthy.length; i++) if (healthy[i] && healthy[i + 1] && healthy[i + 2] && healthy[i + 3]) return { recoveredMs: i * 250, rule: 'first of 4 consecutive 250 ms bins each with painted fps >= 80 % of baseline and p50 latency <= baseline p50 + max(10 ms, 30 %)' };
  return { recoveredMs: null, rule: 'never healthy for 1 s within the recovery phase' };
}

// baseline, condition, recovery in one stamp window; apply() and restore() switch the condition
async function impairedWindow(cl, name, relay, sc, o, { apply = null, restore = null, prefix = 'impair' } = {}) {
  const A = sc.baseline_s || o.baseline_s || 10, B = sc.impaired_s || o.impaired_s || 20, C = sc.recover_s || o.recover_s || 15;
  const total = A + B + C;
  const lbl = prefix + ':' + sc.key;
  if (!apply) apply = async () => { const v = await relay.set({ down: sc.down || {}, up: sc.up || {} }); await relay.arm(sc.schedule || []); return v; };
  if (!restore) restore = async () => { await relay.disarm(); await relay.set({ down: {}, up: {} }); };
  const gate = await loadGate(lbl);
  const st = await stageFor(name, { seconds: total + REQ.warmup + 8, stamp: true });
  const ready = await st.ready;
  if (ready.error) { stopStage(st); return { error: 'stage: ' + ready.error }; }
  const lg = o.large !== false ? startStage(ready.gdi, { seconds: total + REQ.warmup + 8, stamp: false, large: true }) : null;
  if (lg) { const lr = await lg.ready; if (lr.error) { stopStage(st); stopStage(lg); return { error: 'large-area stage: ' + lr.error }; } }
  try {
    return await underLock(lbl, async () => {
      if (relay) await relay.set({ down: {}, up: {} });
      await sleep(REQ.warmup * 1000);
      const load = startLoadSampler(); const hs = hostSampler(1000);
      const tot0 = relay ? (await relay.stats(0, true)).totals : null;
      if (relay) await relay.markBaseline();
      const off = R.clock.browserMinusSystemMs;
      const sec = [];
      const t0 = Date.now(); sec.push(await cl.secondary());
      const collect = cl.collectStamps(ready, total * 1000 + 300, { follow: st });
      await sleep(A * 1000 - (Date.now() - t0));
      const tB = Date.now(); sec.push(await cl.secondary());
      const applied = await apply();
      await sleep(B * 1000 - (Date.now() - tB));
      const tC = Date.now(); const restored = await restore(); sec.push(await cl.secondary());
      const got = await collect; const tEnd = Date.now(); sec.push(await cl.secondary());
      const hostSamples = await hs.stop(); const loadSum = load.stop();
      const rs = relay ? await relay.stats(t0 - 1000) : null;
      const ph = (from, to) => stampMetrics(got.samples, { from: from + off, to: to + off, offsetMs: off });
      const pA = ph(t0, tB), pB = ph(tB, tC), pC = ph(tC, tEnd);
      const lastFive = ph(Math.max(tC, tEnd - 5000), tEnd);
      const baseFps = pA.paintedFps || 0, baseP50 = pA.latencyMs ? pA.latencyMs.p50 : null;
      const rec = baseP50 != null ? recovery(got.samples, { endAt: tC, until: tEnd, baseFps, baseP50, offsetMs: off }) : { recoveredMs: null, rule: 'no baseline' };
      const minus = (x, y) => Object.fromEntries(Object.keys(x).map((k) => [k, /^max/.test(k) ? x[k] : x[k] - (y[k] || 0)]));
      const tot = rs ? { down: minus(rs.totals.down, tot0.down), up: minus(rs.totals.up, tot0.up) } : null;
      const acted = !!(tot && (tot.down.lossDrops + tot.up.lossDrops + tot.down.held + tot.up.held + tot.down.queueDrops + tot.up.queueDrops + tot.down.maxQueueBytes + tot.up.maxQueueBytes + tot.down.maxDelayLineBytes + tot.up.maxDelayLineBytes) > 0);
      const invalid = [];
      if (!(baseFps >= 20)) invalid.push(`baseline painted ${baseFps} fps (< 20): no healthy stream to impair`);
      if (relay && !acted) invalid.push('the relay did nothing to the traffic (no drop, hold, queue or delay recorded)');
      if (rs && rs.lateness_ms.wake.p99 != null && rs.lateness_ms.wake.p99 > 2) invalid.push(`relay scheduler late: wake p99 ${rs.lateness_ms.wake.p99} ms (> 2 ms)`);
      const verdict = loadedVerdict(gate, loadSum);
      if (verdict.loaded) invalid.push('machine CPU above the load gate during the window');
      for (const dir of ['down', 'up']) if (applied && applied[dir] && applied[dir].rate_error) invalid.push(`${dir} cap relative to the baseline not applied: ${applied[dir].rate_error}`);
      const tl = impairTimeline(hostSamples, rs && rs.seconds, got.samples, { t0, tB, tC, offsetMs: off });
      const lastHost = hostSamples.filter((x) => !x.error).pop() || {};
      return {
        scenario: sc.key, label: sc.label, impairment: relay ? { down: sc.down || null, up: sc.up || null, schedule: sc.schedule || null } : undefined, applied, restored, seconds: { baseline: A, impaired: B, recovery: C },
        valid: invalid.length === 0, invalid, ...verdict, load: loadSum,
        phases: {
          baseline: { stamps: pA, page: pageDelta(sec[0], sec[1], A), host: hostPhase(hostSamples, t0, tB) },
          impaired: { stamps: pB, page: pageDelta(sec[1], sec[2], B), host: hostPhase(hostSamples, tB, tC) },
          recovery: { stamps: pC, page: pageDelta(sec[2], sec[3], C), host: hostPhase(hostSamples, tC, tEnd) },
        },
        recovery: { ...rec, lastFiveSecondsP50: lastFive.latencyMs ? lastFive.latencyMs.p50 : null, baselineP50: baseP50, backToFloor: lastFive.latencyMs && baseP50 != null ? lastFive.latencyMs.p50 <= baseP50 + Math.max(5, 0.15 * baseP50) : null },
        relay: rs ? { totals: tot, lateness_ms: rs.lateness_ms, schedule: rs.schedule, perSecond: rs.seconds.map((r) => ({ s: Math.round((r.t - tB) / 100) / 10, down: { in: r.down.in, sent: r.down.sent, kbps: Math.round(r.down.sentBytes * 8 / 1000), lossDrops: r.down.lossDrops, queueDrops: r.down.queueDrops, held: r.down.held, queueBytes: r.down.queueBytes }, up: { in: r.up.in, sent: r.up.sent, lossDrops: r.up.lossDrops } })) } : null,
        hostSamples: hostSamples.length, hostRaw: dumpRows(`impair-${sc.key}-${Date.now()}.json`, { hostSamples, relaySeconds: rs && rs.seconds }),
        stream: { encoder: lastHost.encoder || null, captureBackend: lastHost.captureBackend || null, governor: tl.some((r) => r.gov) },
        timeline: tl, timelineTable: timelineTable(tl), retransmitLoad: retransmitLoad(tl),
      };
    });
  } finally { await st.exited.catch(() => {}); stopStage(st); if (lg) { await lg.exited.catch(() => {}); stopStage(lg); } }
}

async function impairTest() {
  const o = REQ.impair === true ? {} : { ...REQ.impair };
  // a known name given as an object with no impairment of its own keeps the table's, with overrides (durations, label)
  const known = (s) => IMPAIRMENTS[s.name] && s.down === undefined && s.up === undefined && s.schedule === undefined;
  const list = (o.scenarios || DEFAULT_IMPAIRMENTS).map((s) => (typeof s === 'string' ? { key: s, ...(IMPAIRMENTS[s] || { error: 'unknown impairment; known: ' + Object.keys(IMPAIRMENTS).join(', ') }) } : known(s) ? { key: s.name, ...IMPAIRMENTS[s.name], ...s } : { key: s.name || 'custom', label: s.label || 'custom', ...s }));
  const spec = { ...REQ.client, ...(o.client || {}) };
  const out = { client: spec, queueDiscipline: QUEUE_DISCIPLINE, scenarios: [] };
  const cl = await openClient(spec, { offset: 4, browser: o.browser || 'chrome', workerExtra: o.timeline ? RX_HOOK_SOURCE : '' });
  let relay = null;
  try {
    relay = await attachRelay(cl, {});
    const j = await joinRetry(cl, spec);
    out.join = { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers, error: j.error };
    out.relay = relay.info(); out.relayVerdict = await relayVerdict(cl, relay);
    if (j.firstPictureMs == null) { out.error = 'no picture after join'; await leave(cl); return out; }
    if (!out.relayVerdict.viaRelay) { out.error = 'media is not going through the relay: no impairment can be applied'; await leave(cl); return out; }
    for (const sc of list) {
      if (sc.error) { out.scenarios.push({ scenario: sc.key, error: sc.error }); continue; }
      const w = await impairedWindow(cl, j.name, relay, sc, o);
      out.scenarios.push(w);
      const P = w.phases || {};
      log(`impair ${sc.key}: valid=${w.valid} base p50 ${P.baseline && P.baseline.stamps.latencyMs.p50} fps ${P.baseline && P.baseline.stamps.paintedFps} | impaired fps ${P.impaired && P.impaired.stamps.paintedFps} gaps>100 ${P.impaired && P.impaired.stamps.gapsOver100ms} | recovered ${w.recovery && w.recovery.recoveredMs} ms${w.error ? ' ERROR ' + w.error : ''}`);
      await sleep((o.settle_s || 3) * 1000);
    }
    await leave(cl);
  } catch (e) { out.error = e.stack || e.message; }
  finally { out.pageConsole = pageConsoleOf(cl); if (relay) { const f = await relay.stop().catch((e) => ({ error: e.message })); out.relayFinal = f && f.totals ? { totals: f.totals, lateness_ms: f.lateness_ms } : f; } await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1); }
  return out;
}

// slow_client: the page's contract is "never schedule, resynchronise", so a client that cannot
// keep up must drop to the newest frame and recover, never build a queue. Each case is a phased
// window (baseline -> slow -> recovery) on one join, except software decode, which needs its
// own browser (a Chrome flag) and is one timed window.
const SLOW_HOOK_SOURCE = `(() => {
  if (self.__seSlow) return;
  const g = self.__seSlow = { spin: 0, spun: 0 };
  for (const C of [self.OffscreenCanvasRenderingContext2D, self.CanvasRenderingContext2D]) {
    if (!C) continue;
    const d0 = C.prototype.drawImage;
    C.prototype.drawImage = function (src) {
      if (g.spin > 0 && typeof VideoFrame !== 'undefined' && src instanceof VideoFrame) { const s = performance.now(); while (performance.now() - s < g.spin) { /* weak-client proxy */ } g.spun++; }
      return d0.apply(this, arguments);
    };
  }
})();`;

async function setCpuThrottle(cl, rate) {
  const r = { page: await cl.page.send('Emulation.setCPUThrottlingRate', { rate }).then(() => 'ok', (e) => e.message), workers: [] };
  for (const sid of [...cl.workers.keys()]) r.workers.push(await cl.page.send('Emulation.setCPUThrottlingRate', { rate }, sid).then(() => 'ok', (e) => e.message));
  return r;
}

async function slowClient() {
  const o = REQ.slow_client === true ? {} : { ...REQ.slow_client };
  const cases = o.cases || ['worker_spin', 'cpu_throttle', 'hidden', 'software_decode'];
  const spin = o.spin_ms || 25, rate = o.cpu_rate || 6;
  const spec = { ...REQ.client, ...(o.client || {}) };
  const out = { client: spec, cases: {} };
  const phased = cases.filter((c) => c !== 'software_decode');
  if (phased.length) {
    const cl = await openClient(spec, { offset: 5, browser: o.browser || 'chrome', workerExtra: SLOW_HOOK_SOURCE });
    try {
      const j = await joinRetry(cl, spec);
      out.join = { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers, error: j.error };
      if (j.firstPictureMs == null) { out.error = 'no picture after join'; }
      else for (const c of phased) {
        let apply, restore, label;
        if (c === 'worker_spin') {
          label = `the video worker busy-waits ${spin} ms before every paint (a decode/paint path slower than the frame rate)`;
          apply = async () => ({ workers: (await workerEval(cl, `self.__seSlow ? (__seSlow.spin = ${spin}, __seSlow.spun = 0, true) : false`)).map((x) => x.value) });
          restore = async () => ({ spun: (await workerEval(cl, 'self.__seSlow ? (__seSlow.spin = 0, __seSlow.spun) : null')).map((x) => x.value) });
        } else if (c === 'cpu_throttle') {
          label = `CDP Emulation.setCPUThrottlingRate ${rate}x on the page and every worker`;
          apply = () => setCpuThrottle(cl, rate); restore = () => setCpuThrottle(cl, 1);
        } else if (c === 'hidden') {
          label = 'the tab is hidden (another tab opened in front through CDP), then shown again';
          let other = null;
          apply = async () => {
            const t = await cl.chrome.browser.send('Target.createTarget', { url: 'about:blank', background: false }).catch((e) => ({ error: e.message }));
            other = t && t.targetId; if (other) await cl.chrome.browser.send('Target.activateTarget', { targetId: other }).catch(() => {});
            await sleep(300);
            return { otherTarget: !!other, visibility: await cl.page.ev('document.visibilityState').catch((e) => 'ERR ' + e.message), hidden: await cl.page.ev('document.hidden').catch(() => null) };
          };
          restore = async () => {
            if (other) await cl.chrome.browser.send('Target.closeTarget', { targetId: other }).catch(() => {});
            await cl.chrome.browser.send('Target.activateTarget', { targetId: cl.page.targetId }).catch(() => {});
            await sleep(300);
            return { visibility: await cl.page.ev('document.visibilityState').catch((e) => 'ERR ' + e.message) };
          };
        } else { out.cases[c] = { error: 'unknown case; known: worker_spin, cpu_throttle, hidden, software_decode' }; continue; }
        const w = await impairedWindow(cl, j.name, null, { key: c, label }, { baseline_s: o.baseline_s || 10, impaired_s: o.slow_s || 15, recover_s: o.recover_s || 15, large: o.large !== false }, { apply, restore, prefix: 'slow' });
        if (c === 'hidden' && w.applied && w.applied.visibility !== 'hidden') { w.valid = false; (w.invalid = w.invalid || []).push(`the page never became hidden (visibilityState ${w.applied.visibility}): headless Chrome did not background it`); }
        if (c === 'cpu_throttle' && w.applied && !(w.applied.workers || []).includes('ok')) { w.valid = false; (w.invalid = w.invalid || []).push('the throttle reached the page only (CDP refuses it for workers), and the video path runs in a worker'); }
        if (c === 'worker_spin' && w.restored && !(w.restored.spun || []).some((x) => x > 0)) { w.valid = false; (w.invalid = w.invalid || []).push('the spin never ran (no worker painted through the hook)'); }
        out.cases[c] = w;
        const P = w.phases || {};
        log(`slow ${c}: valid=${w.valid} base fps ${P.baseline && P.baseline.stamps.paintedFps} p50 ${P.baseline && P.baseline.stamps.latencyMs.p50} | slow fps ${P.impaired && P.impaired.stamps.paintedFps} p50 ${P.impaired && P.impaired.stamps.latencyMs.p50} | recovered ${w.recovery && w.recovery.recoveredMs} ms`);
        await sleep(3000);
      }
      await leave(cl);
    } catch (e) { out.error = e.stack || e.message; }
    finally { out.pageConsole = pageConsoleOf(cl); await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1); }
  }
  if (cases.includes('software_decode')) {
    const cl = await openClient(spec, { offset: 5, browser: o.browser || 'chrome', flags: ['--disable-accelerated-video-decode'] });
    try {
      const j = await joinRetry(cl, spec);
      if (j.firstPictureMs == null) out.cases.software_decode = { error: 'no picture', join: j };
      else {
        const w = await timedWindow(cl, j.name, { seconds: o.slow_s || 15, mover: false, large: o.large !== false, label: 'slow:software_decode' });
        const sec = await cl.secondary();
        out.cases.software_decode = { label: 'Chrome with --disable-accelerated-video-decode (a separate browser and join)', latencyMs: w.latencyMs, paintedFps: w.paintedFps, longestGapMs: w.longestGapMs, gapsOver100ms: w.gapsOver100ms, skippedStamps: w.skippedStamps, loaded: w.loaded, pageCounters: w.pageCounters, decoder: sec.webrtc && sec.webrtc.video ? sec.webrtc.video.decoder : null, decoderHardware: sec.seVideo && sec.seVideo.decoderHardware, error: w.error };
      }
      await leave(cl);
    } catch (e) { out.cases.software_decode = { error: e.stack || e.message }; }
    finally { await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1); }
  }
  return out;
}

const pageConsoleOf = (cl) => cl.console.filter((c) => /warn|error|exception/.test(c.level)).slice(0, 80);

// The page in a browser on another machine (SSH target from the request or SE_TEST_REMOTE_CLIENT):
// one join, the receive timeline per rate, a stamp window, and both ends' counters every 2 s so a
// stream that stops can be told apart from a page that stops painting.
async function remoteClient() {
  const o = REQ.remote_client === true ? {} : { ...REQ.remote_client };
  const target = o.ssh || process.env.SE_TEST_REMOTE_CLIENT;
  if (!target) { R.notRun.push('remote_client'); return { notRun: true, reason: 'no SSH target: set remote_client.ssh or SE_TEST_REMOTE_CLIENT' }; }
  const out = { probe: await probeRemote(target, { chrome: o.chrome_path }) };
  if (!out.probe.reachable || /no-chrome/.test(out.probe.chrome || '')) { out.skipped = out.probe.reachable ? 'no Chrome at the configured path' : 'not reachable over SSH'; return out; }
  const rates = o.rates || [60, 120];
  const spec = { ...REQ.client, ...(o.client || {}), refresh: Math.max(...rates) };
  let rc = null, cl = null; const timeline = []; let sampling = false; let samplerP = null;
  try {
    rc = await startRemoteChrome(target, { chrome: o.chrome_path, flags: o.flags || [], log });
    out.remoteDir = rc.dir;
    cl = new Client({ port: rc.port, profile: null, flags: [], log, remote: { cleanup: rc.cleanup }, workerExtra: RX_HOOK_SOURCE, browser: 'remote', gate: displayGate });
    cl.stampBand = REQ.stamp_band; owned.clients.push(cl);
    await cl.open({ client: spec, observer: { refreshRate: spec.refresh, disable: [] } });
    out.version = cl.chrome.version;
    out.clock = await cl.contextOffset(null);
    const hs = hostSampler(2000);
    sampling = true;
    samplerP = (async () => { while (sampling) { const t = Date.now(); const s = await cl.secondary(); const v = s.webrtc && s.webrtc.video; timeline.push({ t, conn: s.connectionState, pcs: s.openPeerConnections, packets: v && v.packets, lost: v && v.lost, framesReceived: v && v.framesReceived, framesDecoded: v && v.framesDecoded, pli: v && v.pli, nack: v && v.nack, painted: s.seVideo && s.seVideo.framesPainted, videoPath: s.seVideo && s.seVideo.videoPath, keyRequests: s.seVideo && s.seVideo.keyRequests, error: s.error }); await sleep(Math.max(0, 2000 - (Date.now() - t))); } })();
    const j = await joinRetry(cl, spec);
    out.join = { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers, error: j.error, pageError: j.pageError, page: j.page };
    if (j.firstPictureMs != null) {
      const k = await deviceKey(j.name, cl);
      out.windows = [];
      for (const hz of rates) {
        const cmd = await cli(['devices', 'set', k.key, '--refresh-rate', String(hz)]);
        await sleep((o.settle_s || 5) * 1000);
        const w = await rxWindow(cl, j.name, { hz, seconds: o.seconds || 20, large: o.large !== false, label: `remote:receive@${hz}` });
        const s = await timedWindow(cl, j.name, { seconds: o.seconds || 20, mover: false, large: o.large !== false, label: `remote:stamps@${hz}` });
        out.windows.push({ hz, set: cmd.code, receive: w, stamps: { latencyMs: s.latencyMs, paintedFps: s.paintedFps, longestGapMs: s.longestGapMs, gapsOver100ms: s.gapsOver100ms, skippedStamps: s.skippedStamps, decodeErrors: s.decodeErrors, loaded: s.loaded, clockContexts: s.clockContexts, pageCounters: s.pageCounters, error: s.error } });
        log(`remote @${hz}: frames ${w.frames} src ${w.sourceFps} recv->read ${w.receive_to_read_ms ? w.receive_to_read_ms.mean : 'n/a'} | stamps p50 ${s.latencyMs && s.latencyMs.p50} fps ${s.paintedFps} gap ${s.longestGapMs}`);
      }
    }
    sampling = false; await samplerP;
    out.hostSamples = await hs.stop();
    out.stall = stallsIn(timeline);
    await leave(cl);
  } catch (e) { out.error = e.stack || e.message; }
  finally {
    sampling = false; if (samplerP) await samplerP.catch(() => {});
    out.timeline = timeline;
    if (cl) { out.pageConsole = pageConsoleOf(cl); out.closed = await cl.close().catch((e) => ({ error: e.message })); owned.clients.splice(owned.clients.indexOf(cl), 1); }
    else if (rc) out.closed = await rc.cleanup();
    if (out.closed && out.closed.remote && out.closed.remote.left) R.errors.push('a browser was left running on the remote client: ' + out.closed.remote.left);
  }
  return out;
}

/** Spans in a 2 s counter timeline where packets kept arriving but nothing was decoded or painted, or nothing arrived at all. */
function stallsIn(tl) {
  const spans = [];
  for (let i = 1; i < tl.length; i++) {
    const a = tl[i - 1], b = tl[i];
    if (a.packets == null || b.packets == null) continue;
    const pk = b.packets - a.packets, dec = (b.framesDecoded || 0) - (a.framesDecoded || 0), pnt = (b.painted || 0) - (a.painted || 0);
    const kind = pk === 0 ? 'no packets' : (pnt === 0 && (a.painted != null) ? 'packets but nothing painted' : null);
    if (kind) spans.push({ from: a.t, to: b.t, kind, packets: pk, decoded: dec, painted: pnt, conn: b.conn });
  }
  return { spans: spans.length, first: spans.slice(0, 20) };
}

// Each attached display's current mode (EnumDisplaySettings) and DWM's composition rate
// (DwmGetCompositionTimingInfo). Read only.
function displayRates() {
  const src = [
    'Add-Type -TypeDefinition @"',
    'using System; using System.Runtime.InteropServices;',
    'public static class SeDisp {',
    '  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] public struct DEVMODE { [MarshalAs(UnmanagedType.ByValTStr, SizeConst=32)] public string dmDeviceName; public short dmSpecVersion, dmDriverVersion, dmSize, dmDriverExtra; public int dmFields, dmPositionX, dmPositionY, dmDisplayOrientation, dmDisplayFixedOutput; public short dmColor, dmDuplex, dmYResolution, dmTTOption, dmCollate; [MarshalAs(UnmanagedType.ByValTStr, SizeConst=32)] public string dmFormName; public short dmLogPixels; public int dmBitsPerPel, dmPelsWidth, dmPelsHeight, dmDisplayFlags, dmDisplayFrequency; }',
    '  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern bool EnumDisplaySettings(string name, int mode, ref DEVMODE dm);',
    '  [StructLayout(LayoutKind.Sequential, Pack=1)] public struct TIMING { public int cbSize; public uint rnum, rden; public ulong qpcRefreshPeriod; public uint cnum, cden; public ulong a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p,q,r,s,t,u,v,w,x,y,z,aa,bb,cc,dd,ee,ff,gg,hh,ii,jj,kk; }',
    '  [DllImport("dwmapi.dll")] public static extern int DwmGetCompositionTimingInfo(IntPtr h, ref TIMING t);',
    '  public static string Rate(string n) { var d = new DEVMODE(); d.dmSize = (short)Marshal.SizeOf(typeof(DEVMODE)); return EnumDisplaySettings(n, -1, ref d) ? d.dmPelsWidth + "x" + d.dmPelsHeight + "@" + d.dmDisplayFrequency : "?"; }',
    '  public static string Dwm() { var t = new TIMING(); t.cbSize = Marshal.SizeOf(typeof(TIMING)); int hr = DwmGetCompositionTimingInfo(IntPtr.Zero, ref t); return hr == 0 ? (t.rden > 0 ? ((double)t.rnum / t.rden).ToString("0.##") : "?") + " compose " + (t.cden > 0 ? ((double)t.cnum / t.cden).ToString("0.##") : "?") : "hr " + hr; }',
    '}',
    '"@',
    "Add-Type -AssemblyName System.Windows.Forms; $o = @{}; foreach ($s in [System.Windows.Forms.Screen]::AllScreens) { $o[$s.DeviceName] = [SeDisp]::Rate($s.DeviceName) }; $o['dwm'] = [SeDisp]::Dwm(); $o | ConvertTo-Json -Compress",
  ].join('\n');
  try { return JSON.parse(execFileSync('powershell.exe', ['-NoProfile', '-Command', src], { encoding: 'utf8', windowsHide: true, timeout: 30000 }).trim()); } catch (e) { return { error: e.message.slice(0, 200) }; }
}

// Up to N browsers on one host, added one at a time; after each join every session gets a
// simultaneous stamp window. One display creation per browser.
async function multiSession() {
  const o = REQ.multi_session === true ? {} : { ...REQ.multi_session };
  const max = Math.min(4, o.max || 4), seconds = o.seconds || 15;
  const spec = { ...REQ.client, ...(o.client || {}) };
  const out = { max, seconds, client: spec, steps: [], note: 'every stage draws the same stamp format, so a session showing another session\'s display is not detectable from the stamps alone; it would show as stamps that do not stop when its own display does' };
  const clients = [];
  try {
    out.ratesBefore = displayRates();
    for (let n = 1; n <= max; n++) {
      const cl = await openClient(spec, { offset: 10 + n });
      const j = await joinClient(cl, spec);
      clients.push({ cl, j });
      if (j.firstPictureMs == null) { out.steps.push({ sessions: n, error: 'no picture for the new session', join: { answerMs: j.answerMs, offers: j.offers, pageError: j.pageError } }); break; }
      await sleep(2000);
      const rates = displayRates();
      const stages = [];
      for (const c of clients) { const st = await stageFor(c.j.name, { seconds: seconds + REQ.warmup + 6, stamp: true, mover: true }); stages.push({ st, ready: await st.ready }); }
      const res = await underLock(`multi:${n}`, async () => {
        await sleep(REQ.warmup * 1000);
        const load = startLoadSampler(); const from = Date.now() + 200;
        const got = await Promise.all(clients.map((c, i) => (stages[i].ready.error ? Promise.resolve(null) : c.cl.collectStamps(stages[i].ready, seconds * 1000 + 400, { follow: stages[i].st }))));
        const off = R.clock.browserMinusSystemMs; const to = from + seconds * 1000;
        return { load: load.stop(), per: got.map((g, i) => (g ? { name: clients[i].j.name, display: stages[i].ready.gdi, ...pick(stampMetrics(g.samples, { from: from + off, to: to + off, offsetMs: off })) } : { name: clients[i].j.name, error: 'stage: ' + stages[i].ready.error })) };
      });
      for (const s of stages) { await s.st.exited.catch(() => {}); stopStage(s.st); }
      const joins = (await hostJoins()).joins || [];
      const dead = res.per.filter((p) => !p.error && !(p.newStamps > 0));
      out.steps.push({ sessions: n, displayRates: rates, liveJoins: joins.length, monitors: await ourMonitors(), load: res.load, perSession: res.per, sessionsWithoutNewFrames: dead.map((d) => d.name) });
      log(`multi ${n}: ${res.per.map((p) => `${p.name} p50 ${p.latencyMs && p.latencyMs.p50} fps ${p.paintedFps}`).join(' | ')} rates ${JSON.stringify(rates)}`);
    }
  } catch (e) { out.error = e.stack || e.message; }
  finally {
    for (const c of clients.reverse()) { await leave(c.cl).catch(() => {}); await c.cl.close(); owned.clients.splice(owned.clients.indexOf(c.cl), 1); }
    await sleep(2000); out.ratesAfter = displayRates();
  }
  return out;
}
const pick = (m) => ({ latencyMs: m.latencyMs, newStamps: m.newStamps, paintedFps: m.paintedFps, longestGapMs: m.longestGapMs, gapsOver100ms: m.gapsOver100ms, skippedStamps: m.skippedStamps, duplicateFrames: m.duplicateFrames, backwardsStamps: m.backwardsStamps, decodeErrors: m.decodeErrors });

// churn: cycles of join/leave, rejoin, renegotiate, an aborted join and a killed browser, with
// the host's and the driver's counters after every step. Display creations per kind are fixed
// (CHURN_COST) and the run stops before the driver host reaches DRIVER_GATE.
const CHURN_COST = { join_leave: 1, rejoin: 1, reconfig: 1, abort: 1, kill: 1 };
const DRIVER_GATE = { mb: 5 * 1024, threads: 500 };

async function watchDisplays(seconds, label) {
  const tl = []; const t0 = Date.now();
  while (Date.now() - t0 < seconds * 1000) {
    const m = await ourMonitors(); const j = await hostJoins();
    tl.push({ s: Math.round((Date.now() - t0) / 100) / 10, displays: m.monitors ? m.monitors.length : null, joins: j.joins ? j.joins.length : null });
    if (tl.length > 3 && tl.slice(-3).every((x) => x.displays === 0 && x.joins === 0)) break;
    await sleep(1000);
  }
  const firstZero = tl.find((x) => x.displays === 0);
  return { label, timeline: tl, displayGoneAfterS: firstZero ? firstZero.s : null };
}

function handleDetail(pid) {
  try { return JSON.parse(execFileSync('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(HERE, 'handles.ps1'), '-ProcessId', String(pid)], { encoding: 'utf8', windowsHide: true, timeout: 60000 }).trim().split(String.fromCharCode(10)).pop()); } catch (e) { return { error: e.message.slice(0, 200) }; }
}

async function churnTest() {
  const o = REQ.churn === true ? {} : { ...REQ.churn };
  const kinds = o.kinds || ['join_leave', 'rejoin', 'reconfig', 'abort', 'kill'];
  const cycles = o.cycles || 1;
  const spec = { ...REQ.client, ...(o.client || {}) };
  const out = { kinds, cycles, costPerCycle: kinds.reduce((a, k) => a + (CHURN_COST[k] || 0), 0), driverGate: DRIVER_GATE, steps: [] };
  const grace = await (async () => { const j = await hostJoins(); return j.status && j.status.disconnectGraceSecs; })();
  out.disconnectGraceSecs = grace;
  const after = async (label, extra = {}) => {
    const p = await procSample(H.pid); const m = await ourMonitors(); const j = await hostJoins();
    const tel = j.telemetry || {};
    const step = { label, ...extra, displays: m.monitors ? m.monitors.length : null, joins: j.joins ? j.joins.length : null, liveTransportSessions: tel.live_transport_sessions, host: p };
    // after each leave (and at the start): handles by object type and GPU memory per adapter
    if (/^start$|-left$|:join_leave$|:abort$|:kill$/.test(label)) step.detail = handleDetail(H.pid);
    out.steps.push(step); log(`churn ${label}: displays ${step.displays} joins ${step.joins} host ${p.privateMB} MB ${p.handles} h ${p.threads} t udp ${p.udpSockets} | driver ${p.wudfPrivateMB} MB ${p.wudfThreads} t`);
    return step;
  };
  out.start = await after('start');
  let created = 0; let churnClient = null; let setApplied = null;
  try {
    for (let c = 1; c <= cycles; c++) {
      for (const k of kinds) {
        const last = out.steps[out.steps.length - 1].host;
        if (last.wudfPrivateMB > DRIVER_GATE.mb || last.wudfThreads > DRIVER_GATE.threads) { out.stoppedAt = { cycle: c, kind: k, reason: `driver host at ${last.wudfPrivateMB} MB / ${last.wudfThreads} threads` }; return out; }
        // reuse_client: one browser (one device token) for every cycle, so a per-device setting
        // made once (set_args) applies to all later joins
        const reuse = !!o.reuse_client && k !== 'abort' && k !== 'kill';
        const cl = reuse ? (churnClient = churnClient || await openClient(spec, { offset: 20 })) : await openClient(spec, { offset: 20 });
        try {
          if (k === 'abort') {
            const { url, origin } = clientUrl(spec, deviceName(), { https: spec.https !== false });
            await cl.page.send('Page.navigate', { url });
            await cl.chrome.browser.send('Browser.grantPermissions', { origin, permissions: STD_GRANT }).catch(() => {});
            for (let i = 0; i < 75; i++) { if (await cl.page.ev('document.readyState === "complete" && !!document.getElementById("joinForm")').catch(() => false)) break; await sleep(200); }
            for (let i = 0; i < 6; i++) { await cl.sweepModals(); await sleep(150); }
            const posted = new Promise((res) => cl.page.on((d) => { if (d.method === 'Network.requestWillBeSent' && d.params.request.method === 'POST' && /\/whep/.test(d.params.request.url)) res(Date.now()); }));
            await displayGate('join (to be aborted)');
            await cl.page.ev('document.getElementById("joinForm").requestSubmit()', { gesture: true }).catch(() => {});
            let at = null; posted.then((t) => { at = t; });
            for (let i = 0; i < 100 && !at; i++) { await cl.sweepModals(); await sleep(150); }
            await displayGate('close a tab whose join is in flight'); cl.joined = false;
            await cl.chrome.browser.send('Target.closeTarget', { targetId: cl.page.targetId }).catch(() => {});
            const closedAt = Date.now(); created++;
            const w = await watchDisplays(120, 'abort');
            await after(`c${c}:abort`, { offerSeen: !!at, closedAfterOfferMs: at ? closedAt - at : null, watch: w });
            continue;
          }
          const j = await joinClient(cl, spec); created++;
          if (o.set_args && !setApplied) { const key = await deviceKey(j.name, cl); const r = await cli(['devices', 'set', key.key, ...o.set_args]); setApplied = { args: o.set_args, code: r.code, out: (r.stdout + r.stderr).trim().slice(0, 200), afterCycle: c }; out.setArgs = setApplied; }
          // require_picture false: count the join for leak accounting even when nothing painted
          if (j.firstPictureMs == null && o.require_picture !== false) { await after(`c${c}:${k}`, { error: 'no picture', answerMs: j.answerMs, offers: j.offers, neverConnected: await neverConnected(cl, j) }); continue; }
          if (j.firstPictureMs == null) out.joinsWithoutPicture = (out.joinsWithoutPicture || 0) + 1;
          if (k === 'join_leave') { await leave(cl); await after(`c${c}:join_leave`, { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs }); }
          else if (k === 'rejoin') {
            await cl.away(); await sleep(2000);
            const j2 = await joinClient(cl, spec, { name: j.name });
            const mid = await after(`c${c}:rejoin`, { rejoinPictureMs: j2.firstPictureMs });
            await leave(cl); await after(`c${c}:rejoin-left`, { displaysWhileRejoined: mid.displays });
          } else if (k === 'reconfig') {
            const key = await deviceKey(j.name, cl);
            const edits = [];
            for (const args of [['--video-quality', '30'], ['--video-quality', '20'], ['--refresh-rate', '60'], ['--video-scale', '75'], ['--video-scale', '100']]) { const r = await cli(['devices', 'set', key.key, ...args]); edits.push({ args, code: r.code }); await sleep(3000); }
            const mid = await after(`c${c}:reconfig`, { edits });
            await leave(cl); await after(`c${c}:reconfig-left`, { displaysWhileLive: mid.displays });
          } else if (k === 'kill') {
            await displayGate('kill a browser holding a join'); cl.joined = false;
            const pid = cl.chrome.proc.pid; try { process.kill(pid); } catch { /* gone */ }
            const w = await watchDisplays(120, 'kill');
            await after(`c${c}:kill`, { killedPid: pid, watch: w });
          }
        } finally { if (!reuse) { await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1); } }
      }
    }
  } catch (e) { out.error = e.stack || e.message; }
  finally {
    if (churnClient) { await churnClient.close(); owned.clients.splice(owned.clients.indexOf(churnClient), 1); }
    out.displayCreations = created;
    const s0 = out.start.host, s1 = out.steps[out.steps.length - 1].host;
    const d = (k) => (s0[k] != null && s1[k] != null ? +(s1[k] - s0[k]).toFixed(1) : null);
    // one point per completed cycle (the state after its last step), fitted against the cycle index
    const ends = out.steps.filter((s) => s.detail && s.label !== 'start');
    const byCycle = new Map(); for (const s of ends) { const c = Number((/^c(\d+):/.exec(s.label) || [])[1]); if (c) byCycle.set(c, s); }
    const pts = [...byCycle.entries()].sort((a, b) => a[0] - b[0]);
    const series = (get) => slope(pts.map(([c, s]) => [c, get(s)]), 1);
    const gpuKeys = new Set(); for (const [, s] of pts) for (const g of (s.detail.gpu || [])) gpuKeys.add(g.adapter);
    const typeKeys = new Set(); for (const [, s] of pts) for (const k of Object.keys(s.detail.types || {})) typeKeys.add(k);
    out.perCycleSlopes = {
      cycles: pts.length, unit: 'change per cycle, least squares over the state after each cycle, 95 % interval',
      privateMB: series((s) => s.host.privateMB), handles: series((s) => s.host.handles), threads: series((s) => s.host.threads),
      udpSockets: series((s) => s.host.udpSockets), tcpSockets: series((s) => s.host.tcpSockets), driverPrivateMB: series((s) => s.host.wudfPrivateMB), driverHandles: series((s) => s.host.wudfHandles),
      gpu: Object.fromEntries([...gpuKeys].map((a) => [a, { dedicatedMB: series((s) => ((s.detail.gpu || []).find((g) => g.adapter === a) || {}).dedicatedMB), sharedMB: series((s) => ((s.detail.gpu || []).find((g) => g.adapter === a) || {}).sharedMB) }])),
      handlesByType: Object.fromEntries([...typeKeys].map((k) => [k, series((s) => (s.detail.types || {})[k] || 0)]).filter(([, v]) => v.slope)),
    };
    out.endMinusStart = { privateMB: d('privateMB'), handles: d('handles'), threads: d('threads'), udpSockets: d('udpSockets'), tcpSockets: d('tcpSockets'), wudfPrivateMB: d('wudfPrivateMB'), wudfThreads: d('wudfThreads') };
    out.leftAtEnd = { displays: out.steps[out.steps.length - 1].displays, joins: out.steps[out.steps.length - 1].joins };
  }
  return out;
}

// One session on sustained content for `minutes`; every `period_s` a stamp window plus host,
// process and page counters, appended to soak.jsonl as it goes. Drift and leaks are reported as
// least-squares slopes per hour with a 95 % interval.
async function soakTest() {
  const o = REQ.soak === true ? {} : { ...REQ.soak };
  const minutes = o.minutes || 60, period = o.period_s || 5;
  const spec = { ...REQ.client, ...(o.client || {}) };
  const file = path.join(RUN_DIR, 'soak.jsonl');
  const out = { minutes, periodS: period, file, client: spec };
  const cl = await openClient(spec, { offset: 30 });
  const rows = [];
  let st = null, lg = null;
  try {
    const j = await joinRetry(cl, spec);
    out.join = { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers };
    if (j.firstPictureMs == null) { out.error = 'no picture after join'; return out; }
    const total = minutes * 60;
    st = await stageFor(j.name, { seconds: total + 60, stamp: true, mover: o.mover !== false });
    const ready = await st.ready; if (ready.error) { out.error = 'stage: ' + ready.error; return out; }
    if (o.large) { lg = startStage(ready.gdi, { seconds: total + 60, stamp: false, large: true }); await lg.ready; }
    await underLock('soak', async () => {
      await sleep(REQ.warmup * 1000);
      const t0 = Date.now(); let prevSec = await cl.secondary(); let prevHost = null;
      while (Date.now() - t0 < total * 1000) {
        const from = Date.now();
        const got = await cl.collectStamps(ready, period * 1000, { follow: st });
        const off = R.clock.browserMinusSystemMs;
        const m = stampMetrics(got.samples, { from: from + off, to: Date.now() + off, offsetMs: off });
        const sec = await cl.secondary();
        const hj = await hostJoins(); const s = hj.telemetry && hj.telemetry.sessions ? Object.values(hj.telemetry.sessions)[0] : null;
        const hostNow = s ? { frames: s.frames_encoded, bytes: s.bytes_sent } : null;
        const p = await procSample(H.pid);
        const row = { t: Date.now(), s: Math.round((Date.now() - t0) / 1000), latencyP50: m.latencyMs.p50, latencyP99: m.latencyMs.p99, paintedFps: m.paintedFps, longestGapMs: m.longestGapMs, gapsOver100ms: m.gapsOver100ms,
          page: pageDelta(prevSec, sec, period), host: hostNow && prevHost ? { encodedFps: +((hostNow.frames - prevHost.frames) / period).toFixed(1), mbps: +((hostNow.bytes - prevHost.bytes) * 8 / period / 1e6).toFixed(2) } : null, proc: p,
          pageHeapMB: await cl.page.ev('performance.memory ? Math.round(performance.memory.usedJSHeapSize / 1048576) : null').catch(() => null) };
        prevSec = sec; prevHost = hostNow;
        rows.push(row); fs.appendFileSync(file, JSON.stringify(row) + '\n');
      }
    });
    const xy = (get) => rows.map((r) => [r.t, get(r)]);
    out.windows = rows.length;
    out.slopesPerHour = {
      hostPrivateMB: slope(xy((r) => r.proc.privateMB)), hostHandles: slope(xy((r) => r.proc.handles)), hostThreads: slope(xy((r) => r.proc.threads)),
      hostUdpSockets: slope(xy((r) => r.proc.udpSockets)), hostGpuDedicatedMB: slope(xy((r) => r.proc.gpuDedicatedMB)), driverPrivateMB: slope(xy((r) => r.proc.wudfPrivateMB)),
      latencyP50Ms: slope(xy((r) => r.latencyP50)), paintedFps: slope(xy((r) => r.paintedFps)), pageHeapMB: slope(xy((r) => r.pageHeapMB)),
    };
    out.leakSuspects = Object.entries(out.slopesPerHour).filter(([k, v]) => /MB|Handles|Threads|Sockets/.test(k) && v.trend === 'rising').map(([k]) => k);
    out.note = 'slopes are per hour from a least-squares fit over every window; trend is rising/falling only when the 95 % interval excludes zero. Over a short run, startup settling shows as a slope too.';
    await leave(cl);
  } catch (e) { out.error = e.stack || e.message; }
  finally { if (st) stopStage(st); if (lg) stopStage(lg); out.pageConsole = pageConsoleOf(cl); await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1); }
  return out;
}

// real_input: the real cursor and real injection, only with allow_real_input: true from the
// caller. Pointer moves only, never a click or a key; the cursor stays on the test's virtual
// display and is put back.
function realCursor(args) {
  try { return JSON.parse(execFileSync('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(HERE, 'real-cursor.ps1'), ...args], { encoding: 'utf8', windowsHide: true, timeout: 120000 }).trim().split(/\r?\n/).pop()); } catch (e) { return { error: e.message.slice(0, 200) }; }
}
function realCursorAsync(args) {
  return new Promise((resolve) => {
    const p = spawn('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(HERE, 'real-cursor.ps1'), ...args], { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
    owned.stages.push(p); let out = '';
    p.stdout.on('data', (d) => { out += d; }); p.stderr.on('data', (d) => { out += d; });
    p.on('exit', () => { try { resolve(JSON.parse(out.trim().split(/\r?\n/).pop())); } catch { resolve({ error: out.slice(-300) }); } });
  });
}

async function realInput() {
  if (REQ.allow_real_input !== true) { R.notRun.push('real_input'); return { notRun: true, reason: 'real_input needs allow_real_input: true from the caller' }; }
  const o = REQ.real_input === true ? {} : { ...REQ.real_input };
  const cases = o.cases || ['cursor_sweep', 'remote_control'];
  const seconds = o.seconds || 10;
  const spec = { ...REQ.client, ...(o.client || {}) };
  const out = { cases: {}, cursorBefore: realCursor(['-Get']) };
  const cl = await openClient(spec, { offset: 40 });
  try {
    const j = await joinRetry(cl, spec);
    out.join = { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers };
    if (j.firstPictureMs == null) { out.error = 'no picture after join'; return out; }
    const where = await resolveGdi(j.name);
    if (where.error) { out.error = 'cannot find the test display: ' + where.error; return out; }
    if (cases.includes('cursor_sweep')) {
      // painted fps with a still cursor, then with the real cursor sweeping the display
      const still = await timedWindow(cl, j.name, { seconds, mover: false, large: false, label: 'real:still' });
      const tel0 = (await hostJoins()).telemetry;
      const sweepP = realCursorAsync(['-Gdi', where.gdi, '-Seconds', String(seconds + REQ.warmup + 4)]);
      const moving = await timedWindow(cl, j.name, { seconds, mover: false, large: false, label: 'real:sweep' });
      const sweep = await sweepP; const tel1 = (await hostJoins()).telemetry;
      const cf = (t) => { const s = t && t.sessions ? Object.values(t.sessions)[0] : null; return s && s.degradation ? s.degradation.cursor_frames : null; };
      out.cases.cursor_sweep = { display: where.gdi, sweep, still: pick(still), sweeping: pick(moving), hostCursorFrames: cf(tel0) != null && cf(tel1) != null ? cf(tel1) - cf(tel0) : null, valid: !sweep.aborted && !sweep.error, invalid: [sweep.aborted, sweep.error].filter(Boolean) };
      log(`real cursor_sweep: still fps ${still.paintedFps} p50 ${still.latencyMs && still.latencyMs.p50} | sweeping fps ${moving.paintedFps} p50 ${moving.latencyMs && moving.latencyMs.p50} | ${JSON.stringify(sweep)}`);
    }
    if (cases.includes('remote_control')) {
      const k = await deviceKey(j.name, cl);
      const on = await cli(['devices', 'set', k.key, '--control', 'on', '--pointer-scope', 'display']);
      await sleep(2500);
      const before = realCursor(['-Get']);
      cl.page.enableInput();
      const g = await cl.geometry();
      const n = o.moves || 200; const sent = [];
      const logBefore = fs.readFileSync(path.join(RUN_DIR, 'serve.log'), 'utf8').length;
      for (let i = 0; i < n; i++) {
        const x = g.x + g.w * (0.3 + 0.4 * (i % 50) / 50), y = g.y + g.h * (0.4 + 0.2 * Math.floor(i / 50) / Math.max(1, n / 50));
        const t = preciseNow();
        await cl.page.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y, button: 'none' });
        sent.push(t); await sleep(10);
      }
      await sleep(1500);
      const off = await cli(['devices', 'set', k.key, '--control', 'off']);
      realCursor(['-Put', `${before.x},${before.y}`]);
      const lines = fs.readFileSync(path.join(RUN_DIR, 'serve.log'), 'utf8').slice(logBefore).split('\n').filter((l) => /input-trace kind=pointer/.test(l));
      const tr = lines.map((l) => { const m = /queue_us=(\d+) inject_us=(\d+) wall_ms=(\d+)/.exec(l); return m ? { queueUs: +m[1], injectUs: +m[2], wall: +m[3] } : null; }).filter(Boolean);
      // pair each dispatched move with the first injection after it (moves are 10 ms apart)
      const lat = []; let k2 = 0;
      for (const t of sent) { while (k2 < tr.length && tr[k2].wall < t - 1) k2++; if (k2 < tr.length && tr[k2].wall - t < 200) { lat.push(tr[k2].wall - t); k2++; } }
      out.cases.remote_control = { controlOn: on.code, controlOff: off.code, dispatched: n, injectedTraceLines: tr.length, paired: lat.length,
        dispatchToInjectMs: dist(lat), hostQueueUs: dist(tr.map((x) => x.queueUs)), hostInjectUs: dist(tr.map((x) => x.injectUs)), cursorRestoredTo: before,
        clocks: 'dispatch: precise system clock before Input.dispatchMouseEvent; injection: the host trace wall_ms (SystemTime, ms resolution); both on this machine',
        note: 'page event -> input channel -> host receive -> SendInput; pairing by order, moves 10 ms apart', valid: lat.length > n / 2, invalid: lat.length > n / 2 ? [] : [`only ${lat.length} of ${n} moves paired with an injection`] };
      log(`real remote_control: ${lat.length}/${n} paired, dispatch->inject p50 ${dist(lat).p50}`);
    }
    await leave(cl);
  } catch (e) { out.error = e.stack || e.message; }
  finally {
    out.cursorAfter = realCursor(['-Put', `${out.cursorBefore.x},${out.cursorBefore.y}`]);
    out.pageConsole = pageConsoleOf(cl); await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1);
  }
  return out;
}

// Which top-level windows answer WM_NULL, every 250 ms (hangprobe.ps1), plus the host's threads
// that own windows once a second. Read-only: it sends WM_NULL and nothing else.
const HANG = { proc: null, samples: [], host: [] };
function hangProbe(on, hostPid) {
  if (!on) { if (HANG.proc) { try { HANG.proc.kill(); } catch { /* gone */ } HANG.proc = null; } return; }
  let buf = '';
  HANG.proc = spawn('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(HERE, 'hangprobe.ps1'), '-Seconds', '3600', '-HostPid', String(hostPid || 0)], { stdio: ['ignore', 'pipe', 'ignore'], windowsHide: true });
  owned.stages.push(HANG.proc);
  HANG.proc.stdout.on('data', (d) => {
    const lines = (buf + d).split(/\r?\n/); buf = lines.pop();
    for (const l of lines) { try { const j = JSON.parse(l); if (j.hp && HANG.samples.length < 40000) HANG.samples.push(j); else if (j.hw && HANG.host.length < 10000) HANG.host.push(j); } catch { /* partial */ } }
  });
}
// One join's view of the probe: the refused period from the host log, every window that did not
// answer inside it, and whether the period ended when the last of them answered again.
function hangView(h0, joinT0, joinT1) {
  if (!HANG.samples.length) return null;
  const ev = (h0 && h0.ev) || {}; const at = (k, i = 0) => (ev[k] && ev[k][i] ? h0.requestAt + ev[k][i][0] : null);
  const refusedFrom = at('notCapturable'), opened = at('captureOpened'), gaveUp = at('itemTries');
  const attach = h0 && h0.displayAttached != null ? h0.requestAt + h0.displayAttached : null;
  const itemOk = at('itemOk');
  const from = refusedFrom || attach || joinT0, to = itemOk || opened || gaveUp || joinT1;
  const all = HANG.samples.filter((x) => x.hp >= joinT0 - 2000 && x.hp <= joinT1);
  const inside = all.filter((x) => x.hp >= from && x.hp <= to);
  const before = all.filter((x) => x.hp < joinT0);
  const key = (w) => w[0];
  const seenBefore = new Set(before.flatMap((x) => x.slow.filter((w) => w[8] < 0).map(key)));
  const wins = new Map();
  for (const x of inside) for (const w of x.slow) if (w[8] < 0) {
    const k = key(w); const e = wins.get(k) || { hwnd: w[0], pid: w[1], tid: w[2], process: w[3], cls: w[4], title: w[5], visible: w[6], hung: w[7], first: x.hp, last: x.hp, samples: 0, alreadyBefore: seenBefore.has(k) };
    e.last = x.hp; e.samples++; e.hung = e.hung || w[7]; wins.set(k, e);
  }
  const out = [];
  for (const e of wins.values()) {
    const after = HANG.samples.find((x) => x.hp > e.last && !x.slow.some((w) => w[0] === e.hwnd && w[8] < 0));
    out.push({ ...e, unresponsiveMs: e.last - e.first, answeredAgainAt: after ? after.hp - from : null, first: e.first - from, last: e.last - from });
  }
  // late answers (answered, but slowly) are kept apart from the ones that never answered
  const late = new Map();
  for (const x of inside) for (const w of x.slow) if (w[8] >= 0) { const e = late.get(w[0]) || { hwnd: w[0], pid: w[1], tid: w[2], process: w[3], cls: w[4], title: w[5], samples: 0, maxMs: 0, sumMs: 0 }; e.samples++; e.maxMs = Math.max(e.maxMs, w[8]); e.sumMs += w[8]; late.set(w[0], e); }
  const transient = out.filter((e) => !e.alreadyBefore);
  const lastAnswer = Math.max(-1, ...transient.map((e) => (e.answeredAgainAt == null ? -1 : e.answeredAgainAt)));
  const threadsAt = (t) => { const x = HANG.host.filter((y) => y.hw <= t).pop() || HANG.host.find((y) => y.hw > t); return x ? x.threads : null; };
  return {
    // any Explorer window not answering in a sweep, over the whole join
    shellUnresponsiveMs: (() => { const xs = all.filter((x) => x.hp >= joinT0); let ms = 0; for (let k = 1; k < xs.length; k++) if (xs[k].slow.some((w) => w[3] === 'explorer' && w[8] < 0)) ms += xs[k].hp - xs[k - 1].hp; return ms; })(),
    attachMs: attach ? attach - joinT0 : null, refusedFromMs: refusedFrom ? refusedFrom - joinT0 : null, refusedToMs: to ? to - joinT0 : null, refusedMs: refusedFrom && to ? to - refusedFrom : null, endedBy: itemOk ? 'item handed out' : opened ? 'capture opened' : gaveUp ? 'gave up waiting' : 'join end',
    samples: inside.length, windows: out.sort((a, b) => b.unresponsiveMs - a.unresponsiveMs).slice(0, 40),
    classes: {
      hung: out.filter((e) => e.hung).map((e) => `${e.process}:${e.cls}:${e.unresponsiveMs}`),
      notHungNotAnswering: out.filter((e) => !e.hung).map((e) => `${e.process}:${e.cls}:${e.unresponsiveMs}${e.alreadyBefore ? '' : ' (new)'}`),
      late: [...late.values()].map((e) => ({ process: e.process, cls: e.cls, tid: e.tid, samples: e.samples, maxMs: e.maxMs, meanMs: Math.round(e.sumMs / e.samples) })).sort((a, b) => b.maxMs - a.maxMs).slice(0, 20),
    },
    // the new monitor's taskbar: the first sweep with a secondary taskbar that was not there before the join
    tray: (() => {
      const pre = new Set((all.find((x) => x.hp >= joinT0 - 2000) || { tray: [] }).tray.map((w) => w[0]));
      const hit = all.find((x) => (x.tray || []).some((w) => !pre.has(w[0])));
      const w = hit ? hit.tray.find((y) => !pre.has(y[0])) : null;
      return { appearedMs: hit ? hit.hp - joinT0 : null, afterAttachMs: hit && attach ? hit.hp - attach : null, window: w, before: [...pre].length };
    })(),
    transientLastAnsweredMs: lastAnswer >= 0 ? lastAnswer : null, periodMs: to - from,
    hostThreads: { before: threadsAt(joinT0), during: threadsAt((from + to) / 2), after: threadsAt(to + 1500) },
  };
}

// NVENC sessions on the machine, every 200 ms (`nvidia-smi --query-gpu ... -lms 200`). One join
// at a time runs here, so more than one session means two encoders alive for one join.
const ENC = { proc: null, samples: null };
function encoderSessions(on) {
  if (!on) { if (ENC.proc) { try { ENC.proc.kill(); } catch { /* gone */ } ENC.proc = null; } return; }
  ENC.samples = []; let buf = '';
  ENC.proc = spawn('nvidia-smi', ['--query-gpu=encoder.stats.sessionCount', '--format=csv,noheader', '-lms', '200'], { stdio: ['ignore', 'pipe', 'ignore'] });
  ENC.proc.on('error', (e) => log('nvidia-smi: ' + e.message));
  ENC.proc.stdout.on('data', (d) => { buf += d; let k; while ((k = buf.indexOf('\n')) >= 0) { const v = parseInt(buf.slice(0, k), 10); buf = buf.slice(k + 1); if (isFinite(v)) ENC.samples.push([Date.now(), v]); } });
}

// Removes fields from the JSON body of every POST /whep, so the host sees an offer from a page
// that does not ask for something (an older page, or one with a feature turned off).
function stripOfferShim(fields) {
  return `(function () { if (window.top !== window) return; var real = window.fetch, drop = ${JSON.stringify(fields)};
    window.fetch = function (u, opts) {
      var url = typeof u === 'string' ? u : (u && u.url) || '';
      if (opts && opts.method === 'POST' && /\\/whep$/.test(url.split('?')[0]) && typeof opts.body === 'string') {
        try { var b = JSON.parse(opts.body); for (var i = 0; i < drop.length; i++) delete b[drop[i]]; opts = Object.assign({}, opts, { body: JSON.stringify(b) }); } catch (e) { /* not JSON */ }
      }
      return real.call(this, u, opts);
    }; })();`;
}

// What the page shows, sampled while a join runs: the connecting overlay and its text, the join
// form, the error line, the stage, and the video path the page says it is on.
const PAGE_STATE = `(() => { const vis = (e) => !!(e && e.offsetParent !== null); const l = document.getElementById('loading'); const e = document.getElementById('errorMessage');
  let vp = null; try { vp = window.SEVideo ? SEVideo.getStats().videoPath : null; } catch (x) { vp = 'err'; }
  // the <video> path paints without the stamp hook: count what the element itself presented
  let vid = null; const ve = Array.prototype.slice.call(document.querySelectorAll('video')).find((x) => x.srcObject || x.src);
  if (ve) {
    if (!ve.__seRvfc && ve.requestVideoFrameCallback) { ve.__seRvfc = { n: 0 }; const f = () => { ve.__seRvfc.n++; ve.requestVideoFrameCallback(f); }; ve.requestVideoFrameCallback(f); }
    let q = null; try { q = ve.getVideoPlaybackQuality ? ve.getVideoPlaybackQuality().totalVideoFrames : null; } catch (x) { q = null; }
    vid = { w: ve.videoWidth, h: ve.videoHeight, frames: q, rvfc: ve.__seRvfc ? ve.__seRvfc.n : null, visible: vis(ve) };
  }
  return JSON.stringify({ loading: vis(l), text: l && vis(l) ? l.textContent.trim().slice(0, 80) : null, form: vis(document.getElementById('joinForm')), error: e ? e.textContent.trim().slice(0, 160) : '', stage: vis(document.querySelector('video, canvas')), videoPath: vp, vid }); })()`;

// Loses the response to the first POST /whep of a page load: the request reaches the host and the
// host answers, but the page never sees it. 'drop' fails the fetch after `ms` the way a reset
// connection does (the page offers again at once); 'hold' never resolves it, so the page's own
// 10 s timeout fires. The real response is still read, and recorded in `__seLost`.
function loseAnswerShim(l) {
  const mode = JSON.stringify(l.mode || 'hold'), ms = Number(l.ms) || 300;
  return `(function () { if (window.top !== window) return; var used = false, real = window.fetch; window.__seLost = [];
    window.fetch = function (u, opts) {
      var url = typeof u === 'string' ? u : (u && u.url) || '';
      if (used || !opts || opts.method !== 'POST' || !/\\/whep$/.test(url.split('?')[0])) return real.apply(this, arguments);
      used = true; var t0 = Date.now(), p = real.apply(this, arguments);
      p.then(function (r) { __seLost.push({ kind: 'realResponse', status: r.status, ms: Date.now() - t0 }); }, function (e) { __seLost.push({ kind: 'realError', err: String(e), ms: Date.now() - t0 }); });
      return new Promise(function (res, rej) {
        if (opts.signal) opts.signal.addEventListener('abort', function () { __seLost.push({ kind: 'pageAbort', ms: Date.now() - t0 }); rej(new DOMException('The user aborted a request.', 'AbortError')); });
        if (${mode} === 'drop') setTimeout(function () { __seLost.push({ kind: 'dropped', ms: Date.now() - t0 }); rej(new TypeError('Failed to fetch')); }, ${ms});
      });
    }; })();`;
}
// join_reliability: N joins in a row (or two racing), each timed on both sides: the browser's
// network log for the POST (sent, response headers, body) and the host's log for every stage in
// between. A join fails when its answer takes longer than `threshold_ms`, when the page offered
// more than once, or when nothing painted.
const LAG = { samples: [], timer: null };
function startLagSampler() {
  if (LAG.timer) return;
  let expect = Date.now() + 20;
  LAG.timer = setInterval(() => { const now = Date.now(); if (LAG.samples.length < 400000) LAG.samples.push([now, now - expect]); expect = now + 20; }, 20);
  LAG.timer.unref();
}
const lagMax = (from, to) => LAG.samples.reduce((m, [t, l]) => (t >= from && t <= to && l > m ? l : m), 0);

async function joinReliability() {
  const o = REQ.join_reliability === true ? {} : { ...REQ.join_reliability };
  const n = o.joins || 40, threshold = o.threshold_ms || 5000, pauseS = o.pause_between_s || 0;
  const spec = { ...REQ.client, ...(o.client || {}) };
  setCapturePatterns(o.host_patterns);
  startLagSampler();
  const out = { joins: n, thresholdMs: threshold, race: !!o.race, pauseBetweenS: pauseS, browser: o.browser || 'chrome', hostLog: HOSTCTX.logRelay ? 'relay process' : 'handler pipe', pauseReaderMs: o.pause_reader_ms || 0, results: [] };
  let burn = null;
  if (o.cpu_burn) {
    const t = o.cpu_burn.threads || os.cpus().length, secs = o.cpu_burn.seconds || 1800;
    const src = `const { Worker } = require('worker_threads'); for (let i = 0; i < ${t}; i++) new Worker('const e = Date.now() + ${secs * 1000}; while (Date.now() < e) {}', { eval: true }); setTimeout(() => process.exit(0), ${secs * 1000 + 500});`;
    burn = spawn(process.execPath, ['-e', src], { stdio: 'ignore', windowsHide: true }); owned.stages.push(burn); out.cpuBurn = { threads: t, seconds: secs };
  }
  let monSampler = null; const lastNames = []; let rejoinAt = 0; const hangLater = []; let stopNow = false;
  if (o.encoder_sessions) encoderSessions(true);
  if (o.hang_probe) { const hh = owned.hosts[owned.hosts.length - 1]; hangProbe(true, hh ? hh.pid : 0); out.hangProbeHostPid = hh ? hh.pid : null; await sleep(1500); }
  // remote: the page in Chrome on another machine (SSH target), so the client's decoder does not
  // share a GPU with a host that is adding a monitor
  const remoteTarget = o.remote === true ? process.env.SE_TEST_REMOTE_CLIENT : o.remote;
  let clients;
  if (remoteTarget) {
    const probe = await probeRemote(remoteTarget, { chrome: o.remote_chrome_path });
    out.remote = { target: '(set)', probe };
    if (!probe.reachable || /no-chrome/.test(probe.chrome || '')) { out.error = 'remote client not available: ' + JSON.stringify(probe).slice(0, 200); return out; }
    const rc = await startRemoteChrome(remoteTarget, { chrome: o.remote_chrome_path, flags: o.remote_flags || [], log });
    const cl = new Client({ port: rc.port, profile: null, flags: [], log, remote: { cleanup: rc.cleanup }, browser: 'remote', gate: displayGate });
    cl.stampBand = REQ.stamp_band; owned.clients.push(cl);
    await cl.open({ client: spec, observer: { refreshRate: spec.refresh, disable: [] } });
    out.remote.version = cl.chrome.version;
    clients = [cl];
  } else {
    clients = [await openClient(spec, { offset: 50, browser: o.browser || 'chrome' })];
    if (o.race) clients.push(await openClient(spec, { offset: 51, browser: o.browser || 'chrome' }));
  }
  out.loseAnswer = o.lose_answer || null; out.freshDevice = !!o.fresh_device;
  if (o.offer_strip) for (const cl of clients) await cl.page.send('Page.addScriptToEvaluateOnNewDocument', { source: stripOfferShim(o.offer_strip) });
  out.offerStrip = o.offer_strip || null; out.duringWait = o.during_wait || null;
  try {
    for (let i = 1; i <= n; i++) {
      const t0 = Date.now();
      // the first POST of every page load loses its response from here on (a returning device joins once normally first)
      if (o.lose_answer && i === (o.lose_answer.after_first ? 2 : 1)) for (const cl of clients) await cl.page.send('Page.addScriptToEvaluateOnNewDocument', { source: loseAnswerShim(o.lose_answer) });
      // alternate_device: odd joins are a new device (new token, new name), even joins return as
      // the one before them (its token and its name)
      const fresh = o.alternate_device ? i % 2 === 1 : (o.fresh_device === true || (o.fresh_device === 'first' && i === 1));
      // a first-time device: no token, no join cookie
      // on the page's own session: the browser-level call answers "Internal error" and clears nothing
      if (fresh) for (const cl of clients) {
        const origin = clientUrl(spec, 'x', { https: spec.https !== false }).origin;
        const r1 = await cl.page.send('Storage.clearDataForOrigin', { origin, storageTypes: 'local_storage,cookies' }).then(() => 'ok', (e) => e.message);
        const r2 = await cl.page.send('Network.clearBrowserCookies').then(() => 'ok', (e) => e.message);
        const left = await cl.page.ev('(() => { try { return localStorage.length; } catch (e) { return -1; } })()').catch(() => null);
        if (r1 !== 'ok' || r2 !== 'ok' || left) log(`clear storage: origin ${r1}, cookies ${r2}, localStorage entries left ${left}`);
      }
      if (o.pause_reader_ms && H.pauseReader) setTimeout(() => H.pauseReader(o.pause_reader_ms), o.pause_reader_after_ms || 500);
      // a page that has painted no stamp this long into the join gets one screenshot
      const shotTimers = o.screenshot_slow_s ? clients.map((cl, c) => setTimeout(async () => {
        try { const n = await cl.page.ev('self.__seStamp ? __seStamp.painted : 0').catch(() => 0); const b = await cl.page.send('Page.captureScreenshot', { format: 'jpeg', quality: 40 }); fs.writeFileSync(path.join(RUN_DIR, `slow-${i}-${c}.jpg`), Buffer.from(b.data, 'base64')); log(`join ${i}: screenshot at ${o.screenshot_slow_s} s (painted ${n})`); } catch { /* page gone */ }
      }, o.screenshot_slow_s * 1000)) : [];
      // the monitor list every 250 ms from before the submit, independent of the stamp stage
      if (monSampler) stopStage(monSampler);
      monSampler = o.monitor_samples === false ? null : startStage('', { seconds: 60, stamp: false, extra: ['-MonitorOnly'] });
      // a rapid rejoin is the same device under the same name; its page is already loaded
      const same = (o.alternate_device && !fresh) || (o.rejoin_gaps && i > 1);
      // the page's state every 400 ms, kept only when it changes, and an optional action once the
      // answer is in but before the picture (a tab closed, a reload, a settings edit)
      const pageStates = clients.map(() => []); const actions = clients.map(() => null); let sampling = true; const videoPainted = clients.map(() => null);
      const samplers = clients.map(async (cl, c) => {
        let last = '', acted = false; const ts = Date.now();
        while (sampling) {
          const v = await cl.page.ev(PAGE_STATE).catch(() => null);
          if (v) {
            const st = JSON.parse(v); const key = JSON.stringify({ ...st, vid: st.vid ? { w: st.vid.w, painting: (st.vid.rvfc || st.vid.frames || 0) > 0, visible: st.vid.visible } : null });
            if (key !== last) { pageStates[c].push({ t: Date.now(), ...st }); last = key; }
            if (st.vid && (st.vid.rvfc || st.vid.frames) && !videoPainted[c]) videoPainted[c] = { t: Date.now(), ...st.vid };
          }
          const w = o.during_wait;
          if (w && !acted && (!w.every || i % w.every === 0)) {
            const ans = cl.whep.find((x) => x.method === 'POST' && x.sent >= ts - 100 && x.answered);
            if (ans && Date.now() >= ans.answered + (w.after_answer_ms ?? 300)) {
              acted = true; const at = Date.now(); let r;
              if (w.action === 'reload') r = await cl.page.send('Page.reload').then(() => 'ok', (e) => e.message);
              else if (w.action === 'close') r = await cl.page.send('Page.close').then(() => 'ok', (e) => e.message);
              else if (w.action === 'refresh_rate') { const k = await deviceKey(lastNames[c] || '', cl).catch(() => null); r = k ? (await cli(['devices', 'set', k.key, '--refresh-rate', String(w.value || 120)])).code : 'no device key'; }
              actions[c] = { action: w.action, at, afterAnswerMs: at - ans.answered, result: r };
              log(`join ${i}: ${w.action} during the wait, ${at - ans.answered} ms after the answer: ${r}`);
              if (w.action === 'close') break;
            }
          }
          await sleep(400);
        }
      });
      // deep: the host's counters and the page's WebRTC counters every second, the GPU process before and after
      let deep = null;
      if (o.deep) {
        const gpu = async (cl) => {
          const p = await cl.chrome.browser.send('SystemInfo.getProcessInfo').then((r) => r.processInfo.filter((x) => x.type === 'GPU' || x.type === 'gpu-process').map((x) => ({ id: x.id, type: x.type, cpuTime: x.cpuTime })), (e) => ({ error: e.message }));
          const i = await cl.chrome.browser.send('SystemInfo.getInfo').then((r) => ({ devices: (r.gpu.devices || []).map((d) => `${d.vendorString || d.vendorId} ${d.deviceString || d.deviceId} ${d.driverVersion || ''}`), status: r.gpu.featureStatus ? { video_decode: r.gpu.featureStatus.video_decode, gpu_compositing: r.gpu.featureStatus.gpu_compositing } : null }), (e) => ({ error: e.message }));
          return { processes: p, info: i };
        };
        deep = { gpuBefore: await gpu(clients[0]), hs: hostSampler(1000), page: [], on: true };
        deep.pageLoop = (async () => { while (deep.on) { const t = Date.now(); const s = await clients[0].secondary(); const v = s.webrtc && s.webrtc.video; deep.page.push({ t, conn: s.connectionState, packets: v && v.packets, lost: v && v.lost, framesReceived: v && v.framesReceived, framesDecoded: v && v.framesDecoded, keyFramesDecoded: v && v.keyFramesDecoded, pli: v && v.pli, fir: v && v.fir, nack: v && v.nack, decoder: v && v.decoder, painted: s.seVideo && s.seVideo.framesPainted, videoPath: s.seVideo && s.seVideo.videoPath, keyRequests: s.seVideo && s.seVideo.keyRequests, error: s.error }); await sleep(Math.max(0, 1000 - (Date.now() - t))); } })();
        deep.gpu = gpu;
      }
      const js = await Promise.all(clients.map((cl, c) => joinClient(cl, spec, { ...(same && lastNames[c] ? { name: lastNames[c] } : {}), ...(rejoinAt ? { submitAt: rejoinAt, prepared: true } : {}) })));
      if (deep) {
        // five seconds past the first picture, so the counters show the stream settling
        if (js[0] && js[0].firstPictureMs != null) await sleep(5000);
        deep.on = false; await deep.pageLoop; deep.host = await deep.hs.stop(); deep.gpuAfter = await deep.gpu(clients[0]);
      }
      sampling = false; await Promise.all(samplers);
      js.forEach((j, c) => { j.pageStates = pageStates[c].map((x) => ({ ...x, t: x.t - (j.submitAt || x.t) })); j.waitAction = actions[c]; j.videoElementPainted = videoPainted[c] ? { ...videoPainted[c], t: videoPainted[c].t - (j.submitAt || videoPainted[c].t) } : null; });
      rejoinAt = 0;
      js.forEach((j, c) => { lastNames[c] = j.name; j.freshDevice = fresh; });
      for (const t of shotTimers) clearTimeout(t);
      const lines = readLog(path.join(RUN_DIR, 'serve.log'));
      for (let c = 0; c < js.length; c++) {
        const j = js[c];
        const post = (j.whepRequests || []).filter((w) => w.method === 'POST');
        // a returning device reuses its name: only the requests made since this join's submit
        const host = joinStages(lines, j.name).filter((h) => h.requestAt >= j.submitAt - 1000);
        const h0 = host[0] || null;
        const p0 = post[0] || null;
        const browserSent = p0 ? j.submitAt + p0.sentMs : null;
        const rec = {
          i, client: c, name: j.name, freshDevice: j.freshDevice, submitLateMs: j.submitLateMs ?? null, pageStates: j.pageStates, waitAction: j.waitAction, videoElementPainted: j.videoElementPainted, answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers, pageError: j.pageError || null,
          post: post.map((w) => ({ sentMs: w.sentMs, answeredMs: w.answeredMs, failedMs: w.failedMs ?? null, status: w.status, failed: w.failed || null, protocol: w.protocol, timing: w.timing || null })),
          host, hostRequests: host.length,
          // positive: the request reached the host's log that long after the browser sent it
          sentToHostLogMs: h0 && browserSent ? Math.round(h0.requestAt - browserSent) : null,
          hostAnswerToBrowserMs: h0 && h0.answer !== undefined && p0 && p0.answeredMs != null ? Math.round(j.submitAt + p0.answeredMs - (h0.requestAt + h0.answer)) : null,
          handlerLoopMaxLagMs: lagMax(t0, Date.now()),
          // where the stamp stage drew, and what the paint hook saw before the first stamp
          stage: j.stage ? { gdi: j.stage.gdi, via: j.stage.via, rect: [j.stage.x, j.stage.y, j.stage.width, j.stage.height] } : null,
          restaged: j.restagedFor ? { stage2: j.stage2 ? { gdi: j.stage2.gdi, via: j.stage2.via } : null } : null,
          stageUpdates: j.stageUpdates || 0, hookBeforeFirstPicture: j.hookBeforeFirstPicture || null, firstStampAgeMs: j.firstStampAgeMs ?? null,
          submitAt: j.submitAt, stageTicks: j.stageTicks,
          // first second in which the stamp stage drew on the join's monitor; first-picture time measured from it
          stageVerifiedAt: ((j.stageTicks || []).find((x) => x.draws > 0 && x.path) || {}).t ?? null, monitorChanges: j.monitorChanges, displayMessages: j.displayMessages,
        };
        // from the host log and the network log alone, so a join that never paints a stamp still reports them
        const sum = (k) => host.reduce((a, h) => a + (h[k] || 0), 0);
        const rel = (h, ms) => (ms == null ? null : Math.round(h.requestAt - (h0 ? h0.requestAt : h.requestAt) + ms));
        const hf = host.find((x) => x.hostFirstFrameMs != null);
        rec.capture = {
          tries: sum('captureTries'), staleRetries: sum('staleRetries'), itemTries: host.map((h) => h.itemTries),
          openedMs: h0 ? h0.captureOpenedMs : null,
          // relative to the first request, also when a re-offer opened the capture
          hostFirstFrameMs: hf ? rel(hf, hf.hostFirstFrameMs) : null,
          firstFrameBackend: (host.find((x) => x.firstFrameBackend) || {}).firstFrameBackend || null,
          firstFrameReopens: sum('firstFrameReopens'), answeredAsIs: host.some((h) => h.answeredAsIs),
          telemetryFirstFrameReopens: Math.max(0, ...host.map((h) => h.telemetryReopens || 0)), telemetryDarkRebuilds: Math.max(0, ...host.map((h) => h.telemetryDark || 0)),
          reopens: host.flatMap((h) => (h.reopens || []).map((t) => rel(h, t))), renegotiations: Math.max(0, host.length - 1),
        };
        rec.capture.reopened = rec.capture.reopens.length > 0 || rec.capture.firstFrameReopens > 0 || rec.capture.telemetryDarkRebuilds > 0;
        rec.scale = host.map((h) => h.scale).filter(Boolean);
        rec.answerToResponseMs = host.map((h) => h.answerToResponseMs);
        rec.hostRequestToResponseMs = host.map((h) => h.requestToResponseMs);
        rec.handlerResumedMs = host.map((h) => h.handlerResumedMs);
        rec.browserRequestToResponseMs = post.map((w) => (w.answeredMs != null && w.sentMs != null ? w.answeredMs - w.sentMs : null));
        // what the host said about a second offer of the same attempt, and when the display went
        rec.attempts = host.flatMap((h) => Object.entries(h.ev || {}).filter(([k]) => ['stillBuilding', 'refusedSecond', 'superseded', 'closedWithoutClient', 'displayRemoved'].includes(k)).flatMap(([k, v]) => v.map((x) => [k, rel(h, x[0])])));
        rec.continuedJoin = host.some((h) => h.continued);
        // the most of our monitors seen at once during the join: a second display for one join shows here
        rec.maxMonitors = Math.max(0, ...(j.monitorChanges || []).map((m) => (m.list || []).length));
        rec.firstPictureAfterStageMs = rec.stageVerifiedAt != null && j.firstPictureMs != null ? Math.round(j.submitAt + j.firstPictureMs - rec.stageVerifiedAt) : null;
        if (ENC.samples) { const w = ENC.samples.filter((x) => x[0] >= t0 && x[0] <= Date.now()); rec.encoderSessionsMax = Math.max(0, ...w.map((x) => x[1])); }
        if (o.hang_probe) { const a = [h0, t0, Date.now()]; hangLater.push(() => { rec.hang = hangView(...a); }); }
        // everything the page and its workers logged during this join, with page times
        rec.console = clients[c].console.filter((x) => x.t >= j.submitAt - 500 && x.t <= Date.now()).map((x) => ({ t: Math.round(x.t - j.submitAt), src: x.src || 'page', level: x.level, text: x.text }));
        rec.media = (clients[c].media || []).filter((x) => x.t >= j.submitAt - 500).map((x) => ({ t: Math.round(x.t - j.submitAt), src: x.src, method: x.method, params: x.params }));
        rec.decoderRestart = rec.console.filter((x) => /decoder restart asked by the page|decoder configured again before its first keyframe/.test(x.text)).map((x) => [x.t, x.text]);
        if (deep && c === 0) {
          rec.deep = { gpuBefore: deep.gpuBefore, gpuAfter: deep.gpuAfter, host: deep.host.map((x) => ({ ...x, t: Math.round(x.t - j.submitAt) })), page: deep.page.map((x) => ({ ...x, t: Math.round(x.t - j.submitAt) })) };
          const gb = JSON.stringify((deep.gpuBefore.processes || []).map((x) => x.id)), ga = JSON.stringify((deep.gpuAfter.processes || []).map((x) => x.id));
          rec.deep.gpuProcessChanged = gb !== ga;
        }
        rec.videoEnd = await clients[c].page.ev(`(() => { try { const s = SEVideo.getStats(); return JSON.stringify({ videoPath: s.videoPath, audioPath: s.audioPath, degraded: s.degraded, tiers: s.tiers || null, renderTier: s.renderTier, transport: s.transport }); } catch (e) { return JSON.stringify({ error: String(e) }); } })()`).then((v) => JSON.parse(v), (e) => ({ evalError: e.message }));
        rec.pageEnd = await clients[c].page.ev(`(() => { const e = document.getElementById('errorMessage'); const f = document.getElementById('joinForm'); const v = document.querySelector('video, canvas'); return { error: e ? e.textContent.trim().slice(0, 200) : null, joinFormVisible: !!(f && f.offsetParent !== null), stageVisible: !!(v && v.offsetParent !== null), connecting: (() => { const l = document.getElementById('loading'); return !!(l && l.offsetParent !== null); })() }; })()`).catch((e) => ({ evalError: e.message }));
        if (o.lose_answer) rec.lost = await clients[c].page.ev('JSON.stringify(self.__seLost || null)').then((v) => JSON.parse(v), () => null);
        rec.failed = [];
        if (j.answerMs == null || j.answerMs > threshold) rec.failed.push(`answer ${j.answerMs} ms (threshold ${threshold})`);
        if (j.offers > 1) rec.failed.push(`${j.offers} offers`);
        if (j.firstPictureMs == null) rec.failed.push('no picture');
        if (rec.failed.length) {
          rec.shape = !host.length ? 'the POST never appeared in the host log'
            : h0.answer === undefined ? 'the host logged the request but no answer for it'
              : rec.hostAnswerToBrowserMs != null && rec.hostAnswerToBrowserMs > 1000 ? 'the host answered but the browser got it late'
                : h0.answer > threshold ? 'the host answered late' : 'other';
        }
        out.results.push(rec);
        if (o.stop_on_fail && rec.failed.length && !(rec.failed.length === 1 && /threshold/.test(rec.failed[0]))) { out.stoppedAt = i; stopNow = true; }
        log(`join_reliability ${i}${js.length > 1 ? '.' + c : ''}: capture tries ${rec.capture.tries}, opened +${rec.capture.openedMs}, host first frame +${rec.capture.hostFirstFrameMs}, reopened ${rec.capture.reopened}, answer->response ${rec.answerToResponseMs.join('/')} ms, browser req->resp ${rec.browserRequestToResponseMs.join('/')} ms`);
        log(`join_reliability ${i}${js.length > 1 ? '.' + c : ''}: answer ${j.answerMs} ms, offers ${j.offers}, picture ${j.firstPictureMs} ms, host answer +${h0 && h0.answer} ms, silence ${h0 && h0.longestSilenceMs} ms, loop lag ${rec.handlerLoopMaxLagMs} ms${rec.failed.length ? ' FAILED: ' + rec.shape : ''}`);
      }
      if (stopNow) { log(`join_reliability: stopping after join ${i}: it failed`); for (const cl of clients) await leave(cl); break; }
      if (o.rejoin_gaps && i <= o.rejoin_gaps.length) {
        // rapid rejoin: the page goes away and the same device comes back `gap` s later, without
        // waiting for the host to take the display down (a reload, or a user toggling a setting)
        const gap = o.rejoin_gaps[i - 1];
        // the leave is a reload of the join page itself (its pagehide sends the leave beacon), so
        // the next submit can go out `gap` s after it with the page already loaded
        const leftAt = Date.now();
        for (const cl of clients) {
          if (cl.joined) { await cl.gate('rejoin'); cl.joined = false; }
          const { url: u } = clientUrl(spec, lastNames[0] || 'x', { https: spec.https !== false });
          await cl.page.send('Page.navigate', { url: u }).catch(() => {});
        }
        out.results[out.results.length - 1].nextGapS = gap;
        rejoinAt = leftAt + gap * 1000;
      } else {
        for (const cl of clients) await leave(cl);
        if (pauseS) await sleep(pauseS * 1000);
        // spaced joins: the next one starts only once Explorer has answered every probe sweep for
        // `quiet_s`, so it measures a host left alone rather than the shell recovering from a leave
        if (o.space_until_shell_quiet && i < n) {
          const q = o.space_until_shell_quiet === true ? {} : o.space_until_shell_quiet;
          const quietMs = (q.quiet_s || 5) * 1000, capMs = (q.cap_s || 60) * 1000, w0 = Date.now();
          let quietSince = null;
          while (Date.now() - w0 < capMs) {
            const x = HANG.samples[HANG.samples.length - 1];
            if (x && x.hp > w0) {
              const busy = x.slow.some((w) => w[3] === 'explorer' && w[8] < 0);
              if (busy) quietSince = null; else if (quietSince == null) quietSince = x.hp;
              if (quietSince != null && x.hp - quietSince >= quietMs) break;
            }
            await sleep(250);
          }
          const rec = out.results[out.results.length - 1];
          rec.spacingWaitMs = Date.now() - w0; rec.spacingCapped = Date.now() - w0 >= capMs;
          log(`join_reliability ${i}: next join after the shell was quiet; waited ${rec.spacingWaitMs} ms${rec.spacingCapped ? ' (cap)' : ''}`);
        }
      }
    }
  } catch (e) { out.error = e.stack || e.message; }
  finally {
    if (burn && burn.exitCode === null) { try { burn.kill(); } catch { /* gone */ } }
    if (monSampler) stopStage(monSampler);
    for (const f of hangLater) { try { f(); } catch (e) { log('hang view: ' + e.message); } }
    if (o.hang_probe) { hangProbe(false); try { fs.writeFileSync(path.join(RUN_DIR, 'hang-probe.json'), JSON.stringify({ samples: HANG.samples, host: HANG.host })); } catch { /* run dir gone */ } }
    if (o.encoder_sessions) { encoderSessions(false); try { fs.writeFileSync(path.join(RUN_DIR, 'encoder-sessions.json'), JSON.stringify(ENC.samples)); } catch { /* run dir gone */ } }
    out.pageConsole = clients.map((cl) => cl.console.slice(0, 300));
    for (const cl of clients) { await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1); }
  }
  const R0 = out.results;
  out.failedJoins = R0.filter((r) => r.failed.length).length;
  out.pass = out.failedJoins === 0 && !out.error;
  out.shapes = R0.filter((r) => r.shape).reduce((a, r) => { a[r.shape] = (a[r.shape] || 0) + 1; return a; }, {});
  out.stages = Object.fromEntries(['answerMs', 'firstPictureMs', 'sentToHostLogMs', 'hostAnswerToBrowserMs', 'handlerLoopMaxLagMs'].map((k) => [k, distOf(R0.map((r) => r[k]))]));
  out.hostStages = Object.fromEntries(STAGE_KEYS.concat(['longestSilenceMs']).map((k) => [k, distOf(R0.map((r) => r.host[0] && r.host[0][k]))]));
  out.capture = {
    tries: distOf(R0.map((r) => r.capture.tries)), staleRetries: distOf(R0.map((r) => r.capture.staleRetries)), openedMs: distOf(R0.map((r) => r.capture.openedMs)),
    hostFirstFrameMs: distOf(R0.map((r) => r.capture.hostFirstFrameMs)), reopenedJoins: R0.filter((r) => r.capture.reopened).length,
    backends: R0.reduce((a, r) => { const k = r.capture.firstFrameBackend || 'none'; a[k] = (a[k] || 0) + 1; return a; }, {}),
    answerToResponseMs: distOf(R0.flatMap((r) => r.answerToResponseMs)), handlerResumedMs: distOf(R0.flatMap((r) => r.handlerResumedMs)),
    hostRequestToResponseMs: distOf(R0.flatMap((r) => r.hostRequestToResponseMs)), browserRequestToResponseMs: distOf(R0.map((r) => r.browserRequestToResponseMs[0])),
    firstPictureAfterStageMs: distOf(R0.map((r) => r.firstPictureAfterStageMs)), maxMonitors: Math.max(0, ...R0.map((r) => r.maxMonitors)),
    encoderSessionsMax: ENC.samples ? Math.max(0, ...ENC.samples.map((x) => x[1])) : null,
  };
  out.firstJoinVsLater = { first: R0[0] ? R0[0].answerMs : null, later: distOf(R0.slice(1).map((r) => r.answerMs)) };
  return out;
}

// A tab closed right after its offer went out: the host should finish the join with nobody to
// answer, then remove it (and its display) on its own. Then one ordinary join checks the host.
async function abandonJoin() {
  const o = REQ.abandon_join === true ? {} : { ...REQ.abandon_join };
  const n = o.joins || 2, closeAfter = o.close_after_ms ?? 50, watchS = o.watch_s || 90;
  const spec = { ...REQ.client, ...(o.client || {}) };
  const out = { joins: n, closeAfterMs: closeAfter, watchS, results: [] };
  for (let i = 1; i <= n; i++) {
    const cl = await openClient(spec, { offset: 60 + i, browser: o.browser || 'chrome' });
    const name = deviceName(); const { origin, url } = clientUrl(spec, name, { https: spec.https !== false });
    const mon = startStage('', { seconds: watchS + 20, stamp: false, extra: ['-MonitorOnly'] });
    const r = { i, name };
    try {
      const t0 = Date.now();
      const jp = cl.join({ url, origin, grant: STD_GRANT, micChoice: 'continue', timeoutMs: 15000 }).catch((e) => ({ error: e.message }));
      let post = null;
      for (let k = 0; k < 400 && !post; k++) { post = cl.whep.find((w) => w.method === 'POST' && w.sent >= t0); if (!post) await sleep(25); }
      if (!post) { r.error = 'no POST went out'; } else {
        await sleep(closeAfter);
        r.closedAt = Date.now(); r.postSentToCloseMs = Math.round(r.closedAt - post.sent);
        r.close = await cl.page.send('Page.close').then(() => 'ok', (e) => e.message);
      }
      await Promise.race([jp, sleep(16000)]);
      await sleep(watchS * 1000 - (Date.now() - (r.closedAt || t0)));
      const host = joinStages(readLog(path.join(RUN_DIR, 'serve.log')), name);
      const h0 = host[0] || null;
      r.hostRequests = host.length;
      r.hostAnswerMs = h0 ? h0.answer ?? null : null;
      r.events = host.flatMap((h) => Object.entries(h.ev || {}).filter(([k]) => ['closedWithoutClient', 'displayRemoved', 'superseded', 'answerReady', 'responseReturned'].includes(k)).flatMap(([k, v]) => v.map((x) => [k, x[0], ...x.slice(1)])));
      // the display, by the independent sampler: when one of ours appeared and when none was left
      const ms = STAGE_MONS.filter((x) => x.mon >= t0);
      const up = ms.find((x) => (x.list || []).length > 0), gone = up ? ms.find((x) => x.mon > up.mon && (x.list || []).length === 0) : null;
      r.displayUpMs = up ? up.mon - (h0 ? h0.requestAt : t0) : null; r.displayGoneMs = gone ? gone.mon - (h0 ? h0.requestAt : t0) : null;
      r.displayGoneAfterCloseMs = gone && r.closedAt ? gone.mon - r.closedAt : null;
      r.maxMonitors = Math.max(0, ...ms.map((x) => (x.list || []).length));
      r.hostLines = h0 ? readLog(path.join(RUN_DIR, 'serve.log')).filter((l) => l.t >= h0.requestAt && l.t <= h0.requestAt + watchS * 1000 && /closed|grace|remov|without a client|leave|session|Failed|Closed/.test(l.text)).slice(0, 30).map((l) => [l.t - h0.requestAt, l.text.slice(0, 160)]) : [];
    } catch (e) { r.error = e.stack || e.message; }
    finally { stopStage(mon); await cl.close().catch(() => {}); owned.clients.splice(owned.clients.indexOf(cl), 1); }
    out.results.push(r);
    log(`abandon_join ${i}: closed ${r.postSentToCloseMs} ms after the POST, display gone +${r.displayGoneMs} ms after the request (${r.displayGoneAfterCloseMs} ms after the close), events ${JSON.stringify(r.events).slice(0, 300)}`);
  }
  if (o.health_join !== false) {
    const cl = await openClient(spec, { offset: 69, browser: o.browser || 'chrome' });
    try { const j = await joinClient(cl, spec); out.healthJoin = { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers }; await leave(cl); }
    catch (e) { out.healthJoin = { error: e.message }; }
    finally { await cl.close().catch(() => {}); owned.clients.splice(owned.clients.indexOf(cl), 1); }
  }
  return out;
}

// chrome://version (edge://version) as the browser reports it: the variation list decides
// field trials, which can change the receive path between two builds of the same Chromium.
async function browserVariations(cl, b) {
  try {
    const t = await cl.chrome.browser.send('Target.createTarget', { url: (b === 'edge' ? 'edge' : 'chrome') + '://version', background: true });
    await sleep(1500);
    const tg = (await (await fetch(`http://127.0.0.1:${cl.port}/json/list`)).json()).find((x) => x.id === t.targetId);
    let text = null;
    if (tg) { const { connect } = await import('./lib/cdp.mjs'); const c = await connect(tg.webSocketDebuggerUrl.replace(/^ws:\/\/[^/]+/, `ws://127.0.0.1:${cl.port}`)); const r = await c.send('Runtime.evaluate', { expression: 'document.body.innerText', returnByValue: true }); text = r.result.value; c.close(); }
    await cl.chrome.browser.send('Target.closeTarget', { targetId: t.targetId }).catch(() => {});
    if (!text) return null;
    const lines = text.split(String.fromCharCode(10)).map((l) => l.trim());
    const after = (key) => { const i = lines.findIndex((l) => l.toLowerCase().startsWith(key)); return i < 0 ? null : lines[i].slice(key.length).trim() || lines[i + 1] || null; };
    return { source: after('variations source'), commandLineVariations: after('command-line variations'), active: after('active variations') };
  } catch (e) { return { error: e.message }; }
}

// the headed browser's window onto the virtual display this join created (never the primary)
async function moveWindowToJoinDisplay(cl, name) {
  const st = await stageFor(name, { seconds: 3, stamp: false });
  const r = await st.ready; stopStage(st);
  if (r.error) return { error: 'cannot find the join display: ' + r.error };
  try {
    const w = await cl.chrome.browser.send('Browser.getWindowForTarget', { targetId: cl.page.targetId });
    await cl.chrome.browser.send('Browser.setWindowBounds', { windowId: w.windowId, bounds: { windowState: 'normal' } });
    await cl.chrome.browser.send('Browser.setWindowBounds', { windowId: w.windowId, bounds: { left: r.x, top: r.y, width: r.width, height: r.height } });
    const after = await cl.chrome.browser.send('Browser.getWindowBounds', { windowId: w.windowId });
    return { display: r.gdi, displayRect: [r.x, r.y, r.width, r.height], bounds: after.bounds, visibility: await cl.page.ev('document.visibilityState').catch(() => null) };
  } catch (e) { return { error: e.message }; }
}

async function receiveHold() {
  const o = REQ.receive_hold === true ? {} : { ...REQ.receive_hold };
  const rates = o.rates || [30, 60, 120];
  const browsers = o.browsers || ['chrome', 'edge'];
  const seconds = o.seconds || 30;
  const out = { rates, browsers, seconds, timerResolutionBefore: timerResolution(), perBrowser: {},
    method: 'worker hook on ReadableStreamDefaultReader.read (lib/rx-hook.mjs): read instant, getMetadata().receiveTime, RTP stamp per encoded video frame; no change to the page' };
  // headed: a visible window, started off every screen and moved onto the join's own virtual
  // display, so the browser is a process with a visible window (timer requests honoured).
  const modes = o.modes || ['headless'];
  for (const b0 of browsers) for (const mode of modes) {
    const b = modes.length > 1 ? `${b0}-${mode}` : b0;
    const headed = mode === 'headed';
    const spec = { ...REQ.client, ...(o.client || {}), refresh: Math.max(...rates) };
    let cl;
    try { cl = await openClient(spec, { offset: 3, browser: b0, workerExtra: RX_HOOK_SOURCE, headed, flags: headed ? ['--window-position=-32000,-32000'] : [] }); } catch (e) { out.perBrowser[b] = { skipped: 'browser did not start: ' + e.message }; continue; }
    const res = { version: cl.chrome.version, mode, windows: [] }; out.perBrowser[b] = res;
    res.variations = await browserVariations(cl, b0);
    let relay = null;
    try {
      if (o.relay) relay = await attachRelay(cl, o.relay === true ? {} : o.relay);
      const j = await joinRetry(cl, spec);
      res.join = { answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers, error: j.error, pageError: j.pageError };
      if (relay) { res.relay = relay.info(); res.relayVerdict = await relayVerdict(cl, relay); if (!res.relayVerdict.viaRelay) R.warnings.push(`receive_hold ${b}: media did not go through the relay`); }
      if (j.firstPictureMs == null) { res.error = 'no picture after join'; await leave(cl); continue; }
      if (headed) res.window = await moveWindowToJoinDisplay(cl, j.name);
      const k = await deviceKey(j.name, cl);
      for (const hz of rates) {
        const cmd = await cli(['devices', 'set', k.key, '--refresh-rate', String(hz)]);
        await sleep((o.settle_s || 5) * 1000);
        const mine = ((await hostJoins()).joins || []).filter((x) => x.name === j.name);
        const w = await rxWindow(cl, j.name, { hz, seconds, large: o.large !== false, relay });
        res.windows.push({ hz, set: { code: cmd.code, out: (cmd.stdout + cmd.stderr).trim().slice(0, 160) }, hostJoin: mine.map((x) => ({ joinId: x.joinId, display: x.display })), ...w });
        const h = w.receive_to_read_ms || {};
        log(`receive_hold ${b} @${hz}: frames ${w.frames}, src ${w.sourceFps} fps, read->read pairs ${w.pairs && w.pairs.share}, recv->read p50 ${h.p50} mean ${h.mean} max ${h.max}${w.error ? ' ERROR ' + w.error : ''}`);
      }
      await leave(cl);
    } catch (e) { res.error = e.stack || e.message; }
    finally {
      if (relay) res.relayFinal = await relay.stop().catch((e) => ({ error: e.message }));
      res.pageConsole = pageConsoleOf(cl);
      await cl.close(); owned.clients.splice(owned.clients.indexOf(cl), 1);
    }
  }
  out.timerResolutionAfter = timerResolution();
  return out;
}

function commitHeadroomGb() {
  try {
    const out = execFileSync('powershell', ['-NoProfile', '-Command', "$c=(Get-Counter '\\Memory\\Committed Bytes','\\Memory\\Commit Limit').CounterSamples; $os=Get-CimInstance Win32_OperatingSystem; '{0} {1}' -f (($c[1].CookedValue-$c[0].CookedValue)/1GB), ($os.FreePhysicalMemory/1MB)"], { encoding: 'utf8', windowsHide: true, timeout: 30000 });
    const [commit, phys] = out.trim().split(/\s+/).map(Number);
    return { commitGb: Math.round(commit * 100) / 100, freePhysGb: Math.round(phys * 100) / 100 };
  } catch (e) { return { error: e.message }; }
}
async function waitForCommit(minGb) {
  if (!minGb) return null;
  const t0 = Date.now(); let h = commitHeadroomGb();
  while (h.commitGb != null && h.commitGb < minGb) {
    log(`commit headroom ${h.commitGb} GB < ${minGb} GB: waiting 60 s`);
    await sleep(60000); h = commitHeadroomGb();
  }
  return { ...h, waitedMs: Date.now() - t0, minGb };
}

// SE_TEST_DISPLAY_GATE names a file holding the epoch ms of the last virtual display add, removal
// or mode change made by anyone on the machine. Every such event this handler causes waits until
// at least 60 s after it (SE_TEST_DISPLAY_GATE_MS can only lengthen that), then records itself
// there and in DISPLAY.events.log beside it. Unset, nothing waits.
const GATE_MIN_MS = 60000;
const DISPLAY_EVENTS = [];
async function displayGate(label) {
  const f = process.env.SE_TEST_DISPLAY_GATE;
  if (!f) return null;
  const min = Math.max(GATE_MIN_MS, Number(process.env.SE_TEST_DISPLAY_GATE_MS) || 0);
  const t0 = Date.now();
  const lastOf = () => { try { return Number(fs.readFileSync(f, 'utf8').replace(/\D/g, '')) || 0; } catch { return 0; } };
  // while nothing of ours is attached, the wait is time someone else may need the display lock
  let released = false;
  if (lastOf() + min - Date.now() > 10000 && displayLockHeld && HOSTCTX && H) {
    const m = await ourMonitors().catch(() => ({}));
    if (m.monitors && m.monitors.length === 0) { releaseDisplayLock(); released = true; log(`display gate (${label}): released the display lock while waiting`); }
  }
  let logged = false;
  for (;;) {
    const w = lastOf() + min - Date.now();
    if (w <= 0) break;
    if (!logged) { log(`display gate (${label}): waiting ${Math.ceil(w / 1000)} s`); logged = true; }
    await sleep(Math.min(w, 2000));
  }
  if (released) {
    await takeDisplayLock();
    // someone else may have caused an event during the wait
    for (let w = lastOf() + min - Date.now(); w > 0; w = lastOf() + min - Date.now()) await sleep(Math.min(w, 2000));
  }
  const at = Date.now();
  fs.writeFileSync(f, String(at));
  try { fs.appendFileSync(path.join(path.dirname(f), 'DISPLAY.events.log'), `${new Date(at).toISOString()} test-handler pid ${process.pid} ${label}\n`); } catch { /* best effort */ }
  const e = { label, at, waitedMs: at - t0, releasedLock: released };
  DISPLAY_EVENTS.push(e);
  return e;
}
// devices set flags that re-plug the monitor (resolution, refresh, DPR, orientation, scale)
const MODE_FLAGS = /^--(refresh-rate|scale|orientation|native-dpr|video-scale|auto-orientation)$/;
async function gatedCli(args, timeout) {
  if (args[0] === 'devices' && args[1] === 'set' && args.some((a) => MODE_FLAGS.test(a))) await displayGate('devices set ' + args.slice(3).join(' '));
  return runCli(HOSTCTX.exe, args, { env: cliEnv(), timeout });
}
/** Stop a host; one with a display attached removes it, which is a gated event. */
async function stopHostGated(h, opts) {
  const m = await ourMonitors().catch(() => ({}));
  if (!m.monitors || m.monitors.length) await displayGate('host stop with a display attached');
  return stopHost(h, opts);
}

// SE_TEST_DISPLAY_LOCK names a directory other tools on the machine also mkdir before they create
// a virtual display; a second display changes how DWM composes the first.
let displayLockHeld = null;
async function takeDisplayLock() {
  const dir = process.env.SE_TEST_DISPLAY_LOCK;
  if (!dir) return null;
  const t0 = Date.now(); const maxMs = Number(process.env.SE_TEST_DISPLAY_LOCK_WAIT_S || 1800) * 1000;
  for (;;) {
    try { fs.mkdirSync(dir); break; } catch (e) {
      if (e.code !== 'EEXIST') throw e;
      let owner = ''; try { owner = fs.readFileSync(path.join(dir, 'owner.txt'), 'utf8').trim(); } catch { /* being written */ }
      const dead = /test-handler pid (\d+)/.exec(owner);
      if (dead && !pidAlive(Number(dead[1]))) { log(`display lock left by dead handler ${dead[1]}: removing`); try { fs.rmSync(path.join(dir, 'owner.txt'), { force: true }); fs.rmdirSync(dir); } catch { /* raced */ } continue; }
      if (Date.now() - t0 > maxMs) throw new Error(`display lock ${dir} still held after ${Math.round(maxMs / 1000)} s by: ${owner || '?'}`);
      log(`display lock held by: ${owner || '?'}; retrying in 20 s`); await sleep(20000);
    }
  }
  fs.writeFileSync(path.join(dir, 'owner.txt'), `test-handler pid ${process.pid} ${new Date().toISOString()}
`);
  displayLockHeld = dir;
  return { dir, waitedMs: Date.now() - t0 };
}
function releaseDisplayLock() {
  if (!displayLockHeld) return;
  try { fs.rmSync(path.join(displayLockHeld, 'owner.txt'), { force: true }); fs.rmdirSync(displayLockHeld); } catch { /* someone else's now */ }
  displayLockHeld = null;
}
process.on('exit', releaseDisplayLock);
for (const sig of ['SIGINT', 'SIGTERM', 'SIGBREAK']) process.on(sig, () => { releaseDisplayLock(); process.exit(130); });

let REQ = null;
let STATUS_ONLY = false;

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.gateCheck) {
    // exercises the gate alone: N back-to-back calls, nothing on any display changes
    STATUS_ONLY = true;
    const out = [];
    for (let i = 0; i < args.gateCheck; i++) out.push(await displayGate(`gate-check ${i + 1}`));
    process.stdout.write(JSON.stringify({ gate: process.env.SE_TEST_DISPLAY_GATE || null, events: out }, null, 1) + String.fromCharCode(10));
    return 0;
  }
  if (args.reap) {
    STATUS_ONLY = true;
    // the lock is only needed when there is a monitor to remove
    const st = await driverState();
    if (Array.isArray(st) && st.some((m) => (m.name || '').startsWith('ScreenExtend - SEH'))) await takeDisplayLock();
    try { process.stdout.write(JSON.stringify(await reap({ log, displayLock: null, gate: displayGate }), null, 1) + String.fromCharCode(10)); } finally { releaseDisplayLock(); }
    return 0;
  }
  if (args.status) { process.stdout.write(JSON.stringify(slotStatus(), null, 1) + '\n'); STATUS_ONLY = true; return 0; }
  if (args.calibrateOnly) {
    // the method's floor alone: no host, one slot, one browser
    REQ = { ...DEFAULTS, ...args.request, client: { ...DEFAULTS.client, ...(args.request.client || {}) } };
    const s = await acquireSlot({ label: 'calibrate-only' }, log); R.slot = s.slot;
    try {
      const cl = await openClient(REQ.client);
      R.chrome = { version: cl.chrome.version };
      R.clock = await clockOffset(cl); Client.refOffsetMs = R.clock.browserMinusSystemMs;
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
  // a killed caller (a watchdog) cannot clean up after this run: watch for it and tear down
  if (args.parentPid) {
    const watch = setInterval(async () => {
      if (pidAlive(args.parentPid)) return;
      clearInterval(watch);
      log(`parent ${args.parentPid} is gone: tearing down`);
      R.errors.push(`parent process ${args.parentPid} exited during the run; torn down early`);
      R.cleanup = await cleanup().catch((e) => ({ error: e.message }));
      releaseDisplayLock();
      if (RUN_DIR) { try { fs.writeFileSync(path.join(RUN_DIR, 'result.json'), JSON.stringify(R, null, 1)); } catch { /* best effort */ } }
      process.exit(1);
    }, 2000);
    watch.unref();
  }
  const exe = path.resolve(args.exe);
  if (!fs.existsSync(exe)) { R.errors.push('exe not found: ' + exe); return 2; }
  const build = probeBuild(exe); R.build = build;
  // A shipped build keeps the variable names as strings and reads none of them. Ask the binary
  // where its config is with the isolation directory set, and believe that over the scan.
  if (build.isolationSwitch) {
    const probeDir = fs.mkdtempSync(path.join(os.tmpdir(), 'screenextend-envprobe-'));
    const r = await runCli(exe, ['config', 'path'], { env: { SCREENEXTEND_DATA_DIR: probeDir } });
    const norm = (s) => s.split(String.fromCharCode(92)).join('/').toLowerCase();
    build.readsEnvironment = norm(r.stdout).includes(norm(probeDir));
    fs.rmSync(probeDir, { recursive: true, force: true });
    if (!build.readsEnvironment) {
      build.isolationSwitch = false; build.envCredentials = false;
      R.warnings.push('this build reads none of the SE_* / SCREENEXTEND_* environment variables (a shipped build): credentials go on the command line and host_env has no effect');
    }
  }
  for (const [dll, present] of Object.entries(build.dlls)) if (!present) R.warnings.push(`${dll} is not beside the exe; ${dll === 'libopus.dll' ? 'system audio' : 'the x264 fallback'} cannot load. Copy it from src-tauri/resources/ of that revision.`);
  const mode = build.isolationSwitch && !REQ.force_exclusive ? 'isolated' : 'exclusive';
  R.mode = mode;
  if (mode === 'exclusive') R.warnings.push(build.isolationSwitch ? 'exclusive mode forced by the request' : 'this build has no SCREENEXTEND_DATA_DIR switch: exclusive mode (all slots held, the real data directory, config.json backed up and restored)');

  // Every virtual display leaks commit in the driver host, and a system out of commit crashes
  // the owner's own programs: wait, re-checking every 60 s, until `min_commit_gb` is free.
  R.commitHeadroom = await waitForCommit(REQ.min_commit_gb);

  const info = { exe, label: REQ.label || null };
  let slots;
  if (mode === 'isolated') { const s = await acquireSlot(info, log); slots = [s.slot]; R.queueWaitMs = s.waitedMs; }
  else { const s = await acquireAll(info, log); slots = s.slots; R.queueWaitMs = s.waitedMs; }
  R.slot = slots[0]; R.slotsHeld = slots;
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  // SE_TEST_RUN_ROOT moves run output and browser profiles (both can grow) off the temp drive
  RUN_DIR = path.join(RUN_ROOT, 'runs', `${stamp}-slot${R.slot}-${process.pid}`); fs.mkdirSync(RUN_DIR, { recursive: true });
  R.runDir = RUN_DIR;
  log(`slot ${R.slot} (${mode}), run dir ${RUN_DIR}`);

  const cfgBefore = sha(REAL_CONFIG); R.ownerConfig = { path: REAL_CONFIG, sha256Before: cfgBefore };
  if (cfgBefore) fs.copyFileSync(REAL_CONFIG, path.join(RUN_DIR, 'config.json.before'));
  R.joins = []; R.special = {};
  // display_check: no host starts while a virtual display that is not ours is attached. Ours are
  // only ever created after this point, so any non-primary monitor here belongs to someone else.
  // It is reported, never removed.
  if (REQ.display_check) {
    const scr = screens();
    const mon = (() => { try { const o = execFileSync('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(HERE, 'stage.ps1'), '-MonitorOnly', '-Seconds', '1'], { encoding: 'utf8', windowsHide: true, timeout: 30000 }); return JSON.parse(o.split(/\r?\n/).find((l) => l.includes('"mon"'))).list; } catch (e) { return { error: e.message }; } })();
    R.displayCheck = { screens: scr, nonPrimaryMonitors: mon };
    log(`display check: screens ${scr.join(' ')}; non-primary monitors ${JSON.stringify(mon)}`);
    if (Array.isArray(mon) && mon.length) { R.error = 'refused: a virtual display that is not ours is attached (reported, not removed): ' + JSON.stringify(mon); log(R.error); R.errors.push(R.error); releaseDisplayLock(); releaseSlots(slots); return 1; }
  }
  try {
    const dataDir = mode === 'isolated' ? path.join(ROOT, `data-slot${R.slot}`) : null;
    if (dataDir) { fs.rmSync(dataDir, { recursive: true, force: true }); fs.mkdirSync(dataDir, { recursive: true }); }
    const ports = { http: 18000 + R.slot * 10, https: 18001 + R.slot * 10 };
    HOSTCTX = { exe, dataDir, ports, mode, build, sessionId: randomSessionId(), otp: randomOtp(), hostArgs: REQ.host_args, hostEnv: REQ.host_env, driverBefore: [], logRelay: REQ.host_log === 'relay' };
    if (REQ.arms) HOSTCTX.hostEnv = { ...REQ.host_env, SE_X_SWITCH_FILE: path.join(RUN_DIR, 'x-switches.txt') };
    if (REQ.real_input && REQ.allow_real_input === true) HOSTCTX.hostEnv = { ...HOSTCTX.hostEnv, SE_INPUT_TRACE: '1' };
    const ds = await driverState(); HOSTCTX.driverBefore = Array.isArray(ds) ? ds : [];
    R.driverBefore = ds;
    R.loadBefore = await snapshot();

    R.displayLock = await takeDisplayLock();
    // a handler killed outright (no chance to clean up) leaves its host, browsers and display
    R.reapedBefore = await reap({ log, displayLock: null, gate: displayGate });
    HOSTCTX.screensBefore = screens();
    H = await bootHost('start');
    R.host = { pid: H.pid, args: H.args, credentialsVia: H.credentialsVia, ip: H.ip, ports, dataDir, startupMs: H.startupMs, startup: H.startup, up: H.up };
    if (!H.up) {
      R.host.logTail = H.logTail;
      const lockedOut = /already running/i.test(H.logTail || '');
      throw new Error(`host did not come up on ${H.ip || '?'}:${ports.http}${lockedOut ? ' (it reports another instance holds the lock: no isolation for this build, or a ScreenExtend is already running)' : ''}`);
    }
    if (dataDir) R.host.isolationVerified = fs.existsSync(path.join(dataDir, 'screenextend.lock'));
    // An older `serve` starts its adapter servers first and answers /health ~300 ms before its
    // control server exists: ask again for a moment rather than deciding "no control verbs"
    // (which means a PID stop later).
    let st = await hostJoins();
    for (let i = 0; i < 10 && !st.telemetry && build.controlStats; i++) { await sleep(200); st = await hostJoins(); }
    R.host.statusJson = st.available ? 'telemetry' : (st.status ? 'status only (no telemetry)' : 'unavailable');
    R.host.controlVerbs = !!st.telemetry;

    const cl = await openClient(REQ.client);
    R.chrome = { version: cl.chrome.version, flags: cl.chrome.args.filter((a) => !/^--user-data-dir|^about:/.test(a)) };
    const body = async () => {
      R.clock = await clockOffset(cl); Client.refOffsetMs = R.clock.browserMinusSystemMs;
      if (REQ.calibrate) R.calibration = await calibrate(cl, REQ.client);
      R.scenarios = await standardScenarios(cl, REQ.client);
      if (REQ.mic_prompt) R.special.mic_prompt = await micPrompt();
      if (REQ.two_clients) R.special.two_clients = await twoClients(cl);
      if (REQ.leave_rejoin) R.special.leave_rejoin = await leaveRejoin(cl);
      if (REQ.slow_answer) R.special.slow_answer = await slowAnswer(cl);
      if (REQ.fallback_paths) R.special.fallback_paths = await fallbackPaths();
      if (REQ.mover_stop) R.special.mover_stop = await moverStopTest(cl);
      if (REQ.audio_paths) R.special.audio_paths = await audioPaths();
      if (REQ.join_failures) R.special.join_failures = await joinFailures();
      if (REQ.arms) R.special.arms = await armsTest(cl);
      if (REQ.auto_rejoin) R.special.auto_rejoin = await autoRejoin(cl);
      if (REQ.receive_hold) R.special.receive_hold = await receiveHold();
      if (REQ.impair) R.special.impair = await impairTest();
      if (REQ.slow_client) R.special.slow_client = await slowClient();
      if (REQ.remote_client) R.special.remote_client = await remoteClient();
      if (REQ.multi_session) R.special.multi_session = await multiSession();
      if (REQ.churn) R.special.churn = await churnTest();
      if (REQ.soak) R.special.soak = await soakTest();
      if (REQ.real_input) R.special.real_input = await realInput();
      if (REQ.join_reliability) R.special.join_reliability = await joinReliability();
      if (REQ.abandon_join) R.special.abandon_join = await abandonJoin();
    };
    // The lock spans the whole live-display part unless `overlap_displays` (per-window locking):
    // with two or more virtual displays attached, streams freeze, retarget or slow down (README,
    // "Concurrency"). Host start, browser start and teardown still overlap.
    // WUDFHost leak per display: snapshots at both ends, under that lock, so the delta is this run's own
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
    R.displayEvents = DISPLAY_EVENTS;
    releaseDisplayLock();
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
  c.relaysClosed = 0;
  for (const r of [...owned.relays]) { await r.stop().catch(() => {}); c.relaysClosed++; }
  for (const h of owned.hosts) c.hosts.push(await underLock('host-stop', () => stopHostGated(h, { exe: HOSTCTX.exe, env: HOSTCTX.dataDir ? { SCREENEXTEND_DATA_DIR: HOSTCTX.dataDir } : {}, useStopVerb: !!(R.host && R.host.controlVerbs) })));
  // whatever displays this run added must be gone
  await sleep(1500);
  const st = await driverState();
  if (Array.isArray(st)) {
    const base = new Set((HOSTCTX && HOSTCTX.driverBefore || []).map((m) => m.id));
    let left = st.filter((m) => !base.has(m.id));
    if (HOSTCTX && HOSTCTX.mode === 'isolated') left = left.filter((m) => m.name && m.name.startsWith(`SEH${R.slot}`));
    c.displaysLeftByUs = left.map((m) => ({ id: m.id, name: m.name }));
    if (left.length && HOSTCTX && HOSTCTX.mode === 'exclusive') {
      // a build without a reachable stop verb was stopped by PID and could not remove its display
      await displayGate('driver remove of leftover monitors');
      await driverRemove(left.map((m) => m.id)).catch((e) => { c.driverRemoveError = e.message; });
      await sleep(2500);
      const again = await driverState();
      c.driverRemoved = left.map((m) => m.id);
      c.displaysLeftAfterDriverRemove = Array.isArray(again) ? again.filter((m) => !base.has(m.id)).map((m) => m.id) : again;
    }
    c.driverAfter = st.map((m) => ({ id: m.id, name: m.name }));
  } else c.driverAfter = st;
  c.hostProcessesLeft = owned.hosts.filter((h) => pidAlive(h.pid)).map((h) => h.pid);
  // a browser still shutting down after Browser.close is not left behind
  for (let i = 0; i < 30 && owned.clients.some((cl) => cl.chrome && !cl.chrome.exited && pidAlive(cl.chrome.proc.pid)); i++) await sleep(500);
  c.chromeLeft = owned.clients.filter((cl) => cl.chrome && !cl.chrome.exited && pidAlive(cl.chrome.proc.pid)).map((cl) => cl.chrome.proc.pid);
  return c;
}

const code = await main().catch((e) => { R.errors.push('fatal: ' + (e.stack || e.message)); return 1; });
R.finishedAt = new Date().toISOString();
if (RUN_DIR) { try { fs.writeFileSync(path.join(RUN_DIR, 'handler.log'), LOGS.join('\n')); fs.writeFileSync(path.join(RUN_DIR, 'result.json'), JSON.stringify(R, null, 1)); } catch { /* best effort */ } }
if (!STATUS_ONLY) process.stdout.write(JSON.stringify(R, null, 1) + '\n');
process.exitCode = code;
