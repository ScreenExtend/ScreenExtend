// Real-session end-to-end test for a macOS host: a real `serve` host (this Mac or another Mac
// over SSH) and a real browser client (headless Chrome on THIS Mac running the shipped client
// page), joined over the LAN, measured from both ends. Same CDP approach as run.mjs (the Windows
// harness), which this does not replace.
//
//   node scripts/e2e-headless/run-mac.mjs --host local [--duration 30] [--motion]
//   node scripts/e2e-headless/run-mac.mjs --host user@intel-mac --remote-binary <path> [--duration 30] [--motion]
//
// Options:
//   --host local|<user@ip>     where `serve` runs (default local)
//   --binary <path>            host binary for --host local (default target/bench/ScreenExtend-bench-r3)
//   --remote-binary <path>     host binary on the remote Mac (required with --host <user@ip>)
//                              (--bin is accepted for either)
//   --size WxH                 screen size headless Chrome reports, at device scale factor 1, and so
//                              the size of the display the host creates (default 800x600)
//   --quality <1-51>           pin the encoder quality after the join (`devices set --video-quality`)
//   --adaptive on|off          adaptive quality for that pin (`--adaptive-quality`; default off when
//                              --quality is given). Applied live; the window starts only once the
//                              page paints again and the host's rate_tier has moved
//   --synthetic <workload>     start the host with SE_TEST_HOOKS=1 SE_TEST_SOURCE=<workload> (stress,
//                              scroll, static); FAILS unless the host reports capture_backend
//                              synthetic:<workload>
//   --host-env KEY=VALUE       extra environment for the host process (repeatable; names must start
//                              with SE_); read back from the running process into the JSON
//   --relay-stall MS@EVERY_S   test knob for the relay: every EVERY_S seconds hold ALL host->client
//                              packets for MS milliseconds, then release them in order, back to
//                              back (an induced network stall); counted in client.relay.final.stall
//   --client-override NAME=PATH  serve a local file in place of the host's /NAME (repeatable; e.g.
//                              transform-worker.js=src-tauri/src/streamer/static/transform-worker.js)
//                              through CDP Fetch, so a client-page change can be tried against a
//                              host binary that embeds the old page. Recorded with its sha256.
//   --expect-fps <n>           source frame rate a motion run must paint at (default 60; pass is
//                              >= 90 % of it)
//   --duration <s>             measured window (default 30; local runs are capped at 50 so the
//                              encoder lock is never held longer than ~90 s)
//   --motion                   move the cursor over the session's virtual display while measuring
//                              (skipped, and recorded as skipped, when the host screen is locked)
//   --motion window[:drag|noise|scroll]
//                              real on-screen motion: motion-window.m opens a borderless window
//                              covering the virtual display and animates it at the display's
//                              refresh, so screen capture produces the frames. FAILS on a synthetic
//                              capture backend, a locked screen, or host fresh captures/s under
//                              80 % of the frames the helper presented
//   --motion-launch direct|open  how the helper is started on the host (default: direct locally,
//                              `open` (LaunchServices, GUI session) over SSH)
//   --client <user@ip>         run Chrome on that Mac instead (headless, temp profile there, driven
//                              over CDP through an SSH port forward); the host must be local and is
//                              joined directly at https://<this Mac's LAN address>
//   --cpu-throttle <rate>      CDP Emulation.setCPUThrottlingRate on the page and the video worker
//                              (verified with a timed busy loop in the worker at the end)
//                              NOTE: Chrome refuses it on worker targets ("only supported for
//                              pages"), and the busy-loop check shows the worker unthrottled
//   --worker-spin-ms <ms>      weak-client proxy that does reach the video worker: busy-wait this
//                              long in the worker before every paint
//   --software-decode          Chrome with --disable-accelerated-video-decode
//   --url-param K=V            extra query parameter on the join URL (repeatable; e.g. present=track)
//   --tick                     per-frame timeline: packet receive (getMetadata().receiveTime) -> worker
//                              read -> draw -> expected display (next page rAF/vsync after the draw, or
//                              requestVideoFrameCallback expectedDisplayTime on a <video> tier), with
//                              the read's phase against the page's vsync; summarised in R.tick
//   --jbt0                     set RTCRtpReceiver.jitterBufferTarget = 0 and playoutDelayHint = 0 on
//                              the video receiver, in the same task as addTransceiver
//   --no-script-transform      hide RTCRtpScriptTransform so the page takes createEncodedStreams
//   --window-state normal|minimized   (headed) the Chrome window's state during the window
//   --dc-ping <n>              during the window, send n PINGs (0x60, docs/INPUT-PROTOCOL.md) on the
//                              page's own 'reliable' DataChannel at random gaps of 20-120 ms and time
//                              each round trip to its PONG; reports the RTT distribution and the
//                              phase lock of PONG handling against 64 Hz and 60 Hz (R.dc_ping)
//   --host-series              poll the host's status every second during the window and keep the
//                              session's scalar fields (bytes, frames, hold-push mode, QP, ...)
//   --chrome-flag <flag>       extra Chrome command-line flag (repeatable); --disable-features= and
//                              --enable-features= lists are merged with the harness's own
//   --power                    sample the host once a second during the window: Apple-silicon
//                              PowerTelemetryData (SystemPowerIn / SystemLoad, mW) and host CPU time
//   --http-port/--https-port   host ports (default 18080/18443, so a real instance is untouched)
//   --out <file.json>          result path (default src-tauri/target/bench-results/session-test/)
//   --wait-remote <s>          how long to wait for the remote host to be free (default 3600)
//   --bench-dir <dir>          remote dir whose all.log/live.status gate the start (default: the
//                              binary's own dir)
//   --done-file <path>         remote file that must hold a line starting with DONE before a
//                              remote host starts (default <bench-dir>/all.log)
//   --lock <file>              local encoder lock (default /tmp/sebench/encoder.lock)
//   --headed                   a visible Chrome instead of headless
//   --relay auto|on|off        loopback relay for the client's traffic (default auto: used only
//                              when Chrome cannot reach the host's LAN address, see below; `on`
//                              forces it, also in front of a local host)
//
// The relay. macOS Local Network privacy is per app: a Chrome that has never been granted it gets
// net::ERR_ADDRESS_UNREACHABLE for every LAN address but this Mac's own, while node and curl from
// the same shell are allowed. Rather than raise that prompt, `auto` detects the refusal and puts a
// relay in node: HTTPS is a TCP pipe on 127.0.0.1:<https-port>, and the WHEP answer's host
// candidates are rewritten (CDP Fetch) to 127.0.0.1:<port> with a UDP pipe per port to the host.
// The UDP half runs on its own worker thread (so nothing the harness does on its main thread can
// stall it), with 8 MB socket buffers and no pacing, and counts per direction: packets in,
// forwarded, send errors, and the largest burst seen in any 10 ms window.
// Media then crosses one extra loopback hop in this process; the document records `relay` so a
// number taken through it is never mistaken for a direct one. Granting Chrome "Local Network" in
// System Settings › Privacy & Security removes the need for it.
//
// Exit: 0 pass; 1 harness/join failure; 2 the session ran but a pass criterion failed (no frame
// painted, decode errors > 0, encoder_rate_drops > 0, display size != requested, a requested
// synthetic source the host did not use; with motion also painted fps < 90 % of --expect-fps or
// client frames dropped > 0); 3 host refused (permission / busy).
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Worker } from 'node:worker_threads';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../..');
const LAUNCHER = path.join(here, 'mac-host.sh');
const MOVER_SRC = path.join(here, 'cursor-mover.c');
const LOCAL_MOVER = path.join(repo, 'src-tauri/target/e2e/cursor-mover');
const REMOTE_MOVER_DIR = 'se-e2e'; // under the remote $HOME
const MOTION_WIN_SRC = path.join(here, 'motion-window.m');
const LOCAL_MOTION_WIN = path.join(repo, 'src-tauri/target/e2e/motion-window');
const REMOTE_CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const CHROME = process.env.CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const now = () => new Date().toISOString();

// ---------------------------------------------------------------- arguments
const argv = process.argv.slice(2);
const opt = (k, d) => { const i = argv.indexOf('--' + k); return i >= 0 && i + 1 < argv.length ? argv[i + 1] : d; };
const flag = (k) => argv.includes('--' + k);
const HOST = opt('host', 'local');
const LOCAL = HOST === 'local';
const DURATION = Number(opt('duration', 30));
const MOTION_ARG = (() => {
  const i = argv.indexOf('--motion');
  if (i < 0) return null;
  const v = argv[i + 1];
  if (v && /^window(:(drag|noise|scroll))?$/.test(v)) return { kind: 'window', mode: v.split(':')[1] || 'drag' };
  if (v && /^window/.test(v)) { console.error('--motion window[:drag|noise|scroll]'); process.exit(64); }
  return { kind: 'cursor' };
})();
const MOTION = !!MOTION_ARG;
const CLIENT = opt('client', null);
const CPU_THROTTLE = opt('cpu-throttle', null) == null ? null : Number(opt('cpu-throttle', null));
const SOFTWARE_DECODE = flag('software-decode');
const WORKER_SPIN = Number(opt('worker-spin-ms', 0)) || 0;
const POWER = flag('power');
const URL_PARAMS = [];
argv.forEach((a, i) => { if (a === '--url-param' && i + 1 < argv.length) { if (!/^[a-z]+=[A-Za-z0-9_-]+$/.test(argv[i + 1])) { console.error('--url-param k=v'); process.exit(64); } URL_PARAMS.push(argv[i + 1]); } });
const TICK = flag('tick');
const DC_PING = Number(opt('dc-ping', 0)) || 0;
const HOST_SERIES = flag('host-series');
const JBT0 = flag('jbt0');
const NO_SCRIPT_TRANSFORM = flag('no-script-transform');
const WINDOW_STATE = opt('window-state', null);
const CHROME_FLAGS = [];
argv.forEach((a, i) => { if (a === '--chrome-flag' && i + 1 < argv.length) { if (!/^--[a-z0-9-]+(=[A-Za-z0-9,/:._=-]*)?$/.test(argv[i + 1])) { console.error('--chrome-flag --name[=value]'); process.exit(64); } CHROME_FLAGS.push(argv[i + 1]); } });
const HTTP_PORT = Number(opt('http-port', 18080));
const HTTPS_PORT = Number(opt('https-port', 18443));
const LOCK = opt('lock', '/tmp/sebench/encoder.lock');
const BIN = LOCAL ? opt('binary', opt('bin', path.join(repo, 'src-tauri/target/bench/ScreenExtend-bench-r3')))
  : opt('remote-binary', opt('bin', ''));
if (!BIN) { console.error('--host <user@ip> needs --remote-binary <path>'); process.exit(64); }
const SIZE = opt('size', '800x600');
const [SIZE_W, SIZE_H] = SIZE.split('x').map(Number);
const QUALITY = opt('quality', null);
const ADAPTIVE = opt('adaptive', QUALITY != null ? 'off' : null);
const SYNTHETIC = opt('synthetic', null);
const HOST_ENV = {};
const OVERRIDES = [];
argv.forEach((a, i) => {
  if (a !== '--client-override' || i + 1 >= argv.length) return;
  const m = /^([A-Za-z0-9._-]+\.(?:js|html|css))=(.+)$/.exec(argv[i + 1]);
  if (!m) { console.error(`--client-override ${argv[i + 1]}: expected NAME=PATH`); process.exit(64); }
  const file = path.resolve(m[2]);
  if (!fs.existsSync(file)) { console.error(`--client-override: ${file} not found`); process.exit(64); }
  const body = fs.readFileSync(file);
  OVERRIDES.push({ name: m[1], path: file, bytes: body.length, sha256: crypto.createHash('sha256').update(body).digest('hex'), body, served: 0 });
});
argv.forEach((a, i) => { if (a === '--host-env' && i + 1 < argv.length) { const kvp = argv[i + 1]; const m = /^(SE_[A-Z0-9_]+)=([^\s'"]*)$/.exec(kvp); if (!m) { console.error(`--host-env ${kvp}: expected SE_NAME=value (no spaces or quotes)`); process.exit(64); } HOST_ENV[m[1]] = m[2]; } });
const EXPECT_FPS = Number(opt('expect-fps', 60));
// The device name the page joins with; it is part of the virtual display's identity.
const DEVICE_NAME = opt('device-name', 'MacE2E');
if (!/^\d+x\d+$/.test(SIZE) || !(SIZE_W >= 320 && SIZE_H >= 240)) { console.error('--size must be WxH, e.g. 1920x1080'); process.exit(64); }
if (QUALITY != null && !(/^\d+$/.test(QUALITY) && +QUALITY >= 1 && +QUALITY <= 51)) { console.error('--quality must be 1..51'); process.exit(64); }
if (ADAPTIVE != null && !/^(on|off)$/.test(ADAPTIVE)) { console.error('--adaptive must be on|off'); process.exit(64); }
if (SYNTHETIC != null && !/^[a-z0-9_:-]+$/.test(SYNTHETIC)) { console.error('--synthetic takes a workload name (stress, scroll, static)'); process.exit(64); }
const BENCH_DIR = opt('bench-dir', LOCAL ? null : path.posix.dirname(BIN));
const WAIT_REMOTE = Number(opt('wait-remote', 3600));
const DONE_FILE = opt('done-file', BENCH_DIR ? BENCH_DIR + '/all.log' : null);
if (DONE_FILE && /['\s]/.test(DONE_FILE)) { console.error('--done-file: no quotes or spaces'); process.exit(64); }
// A stall is induced inside the relay, so asking for one forces the relay: under `auto` a Chrome
// that can reach the LAN directly never used it, and the run silently measured no stall at all.
const RELAY = CLIENT ? 'off' : opt('relay', argv.includes('--relay-stall') ? 'on' : 'auto');
const MOTION_LAUNCH = opt('motion-launch', 'auto');
if (CLIENT && HOST !== 'local' && CLIENT !== HOST) { console.error('--client needs a local host, or the same Mac as --host'); process.exit(64); }
if (CLIENT && (opt('relay', null) === 'on' || argv.includes('--relay-stall'))) { console.error('--client joins directly; the relay is not available'); process.exit(64); }
if (CLIENT && /\s|'/.test(CLIENT)) { console.error('--client: no spaces or quotes'); process.exit(64); }
if (CPU_THROTTLE != null && !(CPU_THROTTLE >= 1 && CPU_THROTTLE <= 20)) { console.error('--cpu-throttle 1..20'); process.exit(64); }
if (!/^(auto|direct|open)$/.test(MOTION_LAUNCH)) { console.error('--motion-launch auto|direct|open'); process.exit(64); }
const RELAY_STALL = (() => {
  const v = opt('relay-stall', null);
  if (v == null) return null;
  const m = /^(\d+)@(\d+(?:\.\d+)?)$/.exec(v);
  if (!m || !(+m[1] > 0 && +m[2] > 0 && +m[1] < +m[2] * 1000)) { console.error('--relay-stall MS@EVERY_S, e.g. 150@3'); process.exit(64); }
  return { ms: +m[1], every_s: +m[2] };
})();
if (RELAY_STALL && RELAY === 'off') { console.error('--relay-stall needs the relay (--relay on, or auto on a host Chrome cannot reach)'); process.exit(64); }
if (!(DURATION > 0)) { console.error('--duration must be positive'); process.exit(64); }
// E2E_ALLOW_LONG=1 lifts the cap for a soak, when nothing else needs the encoder lock.
const ALLOW_LONG = process.env.E2E_ALLOW_LONG === '1';
if (LOCAL && DURATION > 50 && !ALLOW_LONG) { console.error('local runs hold the encoder lock; keep --duration <= 50 (or set E2E_ALLOW_LONG=1)'); process.exit(64); }
for (const v of [BIN, HOST]) if (/\s/.test(v)) { console.error('no whitespace allowed in --bin/--host'); process.exit(64); }

// Every run uses this Mac (its host, its Chrome, or both), so every run holds the shared
// encoder lock while it runs: main() re-execs itself under lockf — after waiting for a busy
// remote host, so the lock is never held while idle.
const IN_LOCK = !!process.env.E2E_RUNMAC_IN_LOCK;
const T_START = Date.now();
// A local host creates a virtual display on this Mac and reflows whatever its user has open, so it
// has to be allowed explicitly.
if (LOCAL && process.env.E2E_ALLOW_LOCAL_DISPLAY !== '1') { console.error('refusing --host local: it creates a virtual display on this Mac. Set E2E_ALLOW_LOCAL_DISPLAY=1 only when its user has agreed.'); process.exit(65); }
// Hard ceiling on the time the lock is held: abort (and clean up) at 85 s.
const DEADLINE_MS = LOCAL ? (ALLOW_LONG ? (DURATION + 60) * 1000 : 85_000) : Math.max(10 * 60_000, (DURATION + 120) * 1000);

// ---------------------------------------------------------------- result document
const SID = Array.from(crypto.randomBytes(12), (b) => 'ABCDEFGHJKLMNPQRSTUVWXYZ23456789'[b % 32]).join('');
const OTP = String(crypto.randomInt(0, 1_000_000)).padStart(6, '0');
const hostIp = LOCAL ? lanIp() : HOST.split('@').pop();
const label = LOCAL ? `local-${os.hostname().split('.')[0]}` : `remote-${hostIp}`;
const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
const OUT = path.resolve(opt('out', path.join(repo, 'src-tauri/target/bench-results/session-test', `${label}-${stamp}.json`)));
fs.mkdirSync(path.dirname(OUT), { recursive: true });
const R = {
  schema: 'screenextend-mac-session-e2e/1', started_at: now(), label,
  host: { where: LOCAL ? 'local' : HOST, ip: hostIp, binary: BIN, http_port: HTTP_PORT, https_port: HTTPS_PORT },
  client: { where: CLIENT || 'local', machine: CLIENT ? null : `${os.hostname().split('.')[0]} ${os.arch()} macOS ${os.release()}`, browser: null, headless: !flag('headed') },
  config: { duration_s: DURATION, motion_requested: MOTION, size_requested: SIZE, quality: QUALITY == null ? null : +QUALITY, adaptive: ADAPTIVE, synthetic_requested: SYNTHETIC, host_env: HOST_ENV, client_overrides: OVERRIDES.map((o) => ({ name: o.name, path: o.path, bytes: o.bytes, sha256: o.sha256 })), relay: RELAY, relay_stall: RELAY_STALL, expect_fps: EXPECT_FPS, motion: MOTION_ARG, client: CLIENT, cpu_throttle: CPU_THROTTLE, worker_spin_ms: WORKER_SPIN, software_decode: SOFTWARE_DECODE, power: POWER, headed: flag('headed'), url_params: URL_PARAMS, tick: TICK, jbt0: JBT0, no_script_transform: NO_SCRIPT_TRANSFORM, window_state: WINDOW_STATE, chrome_flags: CHROME_FLAGS },
  screen_locked: null, motion: { requested: MOTION, ran: false },
  steps: [], failures: [], pass: false,
};
const step = (name, v = {}) => { const e = { name, t_ms: Date.now() - T_START, ...v }; R.steps.push(e); console.log(`[${now()}] ${name}`, JSON.stringify(v).slice(0, 400)); };

function lanIp() {
  for (const [name, addrs] of Object.entries(os.networkInterfaces())) {
    if (!/^en\d/.test(name)) continue;
    for (const a of addrs) if (a.family === 'IPv4' && !a.internal) return a.address;
  }
  throw new Error('no LAN IPv4 address on an en* interface');
}

// ---------------------------------------------------------------- running things on the host
// Every host action goes through mac-host.sh, locally or as `ssh <host> bash -s -- …` with the
// script on stdin. Credentials, when needed, are exported at the top of that stdin, never in argv.
function hostRun(args, { env = null, timeoutMs = 60_000 } = {}) {
  return new Promise((resolve) => {
    let child;
    const script = fs.readFileSync(LAUNCHER, 'utf8');
    if (LOCAL) {
      child = spawn('/bin/bash', [LAUNCHER, ...args], { env: { ...process.env, ...(env || {}) } });
      child.stdin.end();
    } else {
      child = spawn('ssh', ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', HOST, 'bash', '-s', '--', ...args]);
      const exports = Object.entries(env || {}).map(([k, v]) => `export ${k}='${String(v).replace(/'/g, '')}'\n`).join('');
      child.stdin.end(exports + script);
    }
    let out = '', err = '';
    child.stdout.on('data', (d) => { out += d; });
    child.stderr.on('data', (d) => { err += d; });
    const t = setTimeout(() => { try { child.kill('SIGKILL'); } catch {} }, timeoutMs);
    child.on('close', (code) => { clearTimeout(t); resolve({ code, out, err }); });
  });
}
const kv = (text) => Object.fromEntries(text.split('\n').filter((l) => /^[A-Z_]+=/.test(l)).map((l) => [l.slice(0, l.indexOf('=')), l.slice(l.indexOf('=') + 1)]));
function sh(cmd, args, input) { const r = spawnSync(cmd, args, { input, encoding: 'utf8', timeout: 60_000 }); return { code: r.status, out: r.stdout || '', err: r.stderr || '' }; }
const ssh = (cmd) => sh('ssh', ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', HOST, cmd]);
const sshC = (cmd) => sh('ssh', ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', CLIENT, cmd]);
// The asynchronous form, for anything that runs while a session is live.
function shA(cmd, args) {
  return new Promise((res) => { const c = spawn(cmd, args); let out = '', err = ''; c.stdout.on('data', (x) => { out += x; }); c.stderr.on('data', (x) => { err += x; }); c.on('close', (code) => res({ code, out, err })); c.stdin.end(); });
}

// ---------------------------------------------------------------- remote: wait until free
async function waitRemoteFree() {
  const until = Date.now() + WAIT_REMOTE * 1000;
  for (;;) {
    const q = ssh(`cd '${BENCH_DIR}' 2>/dev/null; echo "DONE=$(grep -c '^DONE' '${DONE_FILE}' 2>/dev/null)"; echo "BENCH=$(pgrep -f ScreenExtend-bench | tr '\\n' ' ')"; echo "LIVE=$(tr '\\n' '|' < live.status 2>/dev/null)"`);
    if (q.code !== 0) throw new Error('ssh to the host failed: ' + q.err.trim());
    const s = kv(q.out);
    const done = Number(s.DONE || 0) > 0;
    const bench = (s.BENCH || '').trim();
    const live = s.LIVE || '';
    const liveRunning = /unlocked/.test(live) && !/LIVEDONE/.test(live);
    if (done && !bench && !liveRunning) { step('remote-free', { all_log_done: done, live_status: live }); return; }
    if (Date.now() > until) throw new Error(`remote host still busy after ${WAIT_REMOTE}s (DONE=${done}, bench pids=[${bench}], live=${live})`);
    console.log(`[${now()}] remote busy (DONE=${done}, bench pids=[${bench}], live="${live}"); retry in 30 s`);
    await sleep(30_000);
  }
}

// ---------------------------------------------------------------- the cursor mover
function ensureLocalMover() {
  if (fs.existsSync(LOCAL_MOVER) && fs.statSync(LOCAL_MOVER).mtimeMs >= fs.statSync(MOVER_SRC).mtimeMs) return LOCAL_MOVER;
  fs.mkdirSync(path.dirname(LOCAL_MOVER), { recursive: true });
  const r = sh('clang', ['-O2', '-arch', 'arm64', '-arch', 'x86_64', '-mmacosx-version-min=10.15', '-framework', 'CoreGraphics', '-framework', 'CoreFoundation', '-o', LOCAL_MOVER, MOVER_SRC]);
  if (r.code !== 0) throw new Error('cursor-mover build failed: ' + r.err);
  return LOCAL_MOVER;
}
function ensureLocalMotionWin() {
  if (fs.existsSync(LOCAL_MOTION_WIN) && fs.statSync(LOCAL_MOTION_WIN).mtimeMs >= fs.statSync(MOTION_WIN_SRC).mtimeMs) return LOCAL_MOTION_WIN;
  fs.mkdirSync(path.dirname(LOCAL_MOTION_WIN), { recursive: true });
  const r = sh('clang', ['-O2', '-fobjc-arc', '-arch', 'arm64', '-arch', 'x86_64', '-mmacosx-version-min=10.15', '-framework', 'AppKit', '-framework', 'QuartzCore', '-framework', 'CoreVideo', '-framework', 'CoreGraphics', '-o', LOCAL_MOTION_WIN, MOTION_WIN_SRC]);
  if (r.code !== 0) throw new Error('motion-window build failed: ' + r.err);
  return LOCAL_MOTION_WIN;
}
// Copy a helper to the remote by serving it from this Mac: a short-lived http.server bound to the
// LAN address, in a directory holding only that file, fetched with curl over SSH, then stopped.
async function ensureRemoteBin(local, name) {
  const sum = crypto.createHash('sha256').update(fs.readFileSync(local)).digest('hex');
  const remote = `$HOME/${REMOTE_MOVER_DIR}/${name}`;
  const have = ssh(`shasum -a 256 ${remote} 2>/dev/null | cut -d' ' -f1; echo $HOME`);
  const remoteHome = have.out.trim().split('\n').pop();
  if (have.out.startsWith(sum)) return `${remoteHome}/${REMOTE_MOVER_DIR}/${name}`;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'se-mover-serve-'));
  fs.copyFileSync(local, path.join(dir, name));
  const myIp = lanIp();
  const port = await freePort();
  const srv = spawn('python3', ['-m', 'http.server', String(port), '--bind', myIp, '--directory', dir], { stdio: 'ignore' });
  try {
    await sleep(800);
    const r = ssh(`mkdir -p $HOME/${REMOTE_MOVER_DIR} && curl -fsS -o ${remote}.tmp http://${myIp}:${port}/${name} && chmod +x ${remote}.tmp && mv ${remote}.tmp ${remote} && shasum -a 256 ${remote} | cut -d' ' -f1`);
    if (r.code !== 0 || !r.out.startsWith(sum)) throw new Error(name + ' copy failed: ' + (r.err || r.out));
    step('helper-deployed', { remote: `${remoteHome}/${REMOTE_MOVER_DIR}/${name}`, sha256: sum });
  } finally {
    srv.kill('SIGTERM');
    fs.rmSync(dir, { recursive: true, force: true });
  }
  return `${remoteHome}/${REMOTE_MOVER_DIR}/${name}`;
}
const ensureRemoteMover = () => ensureRemoteBin(ensureLocalMover(), 'cursor-mover');
function freePort() {
  return new Promise((res, rej) => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => res(p)); }); s.on('error', rej); });
}


// ---------------------------------------------------------------- loopback relay (see header)
const RELAY_BUF = 8 * 1024 * 1024; // kern.ipc.maxsockbuf on the M1
const RELAY_WORKER = `
const { parentPort, workerData } = require('node:worker_threads');
const dgram = require('node:dgram');
const { performance } = require('node:perf_hooks');
const HOST = workerData.hostIp, BUF = workerData.buf, STALL = workerData.stall;
// Induced stall (test knob): while 'holding', host->Chrome packets queue here and are released
// in order, back to back, when the hold ends.
const stall = { holding: false, stalls: 0, packets_held: 0, bytes_held: 0, max_held_in_one: 0, held: [] };
function releaseHeld() {
  stall.holding = false;
  const q = stall.held; stall.held = [];
  if (q.length > stall.max_held_in_one) stall.max_held_in_one = q.length;
  for (const h of q) h.sock.send(h.msg, h.port, h.addr, sent(dir.down));
}
let stallTimer = null;
function armStall() {
  if (!STALL || stallTimer) return;
  stallTimer = setInterval(() => { stall.stalls++; stall.holding = true; setTimeout(releaseHeld, STALL.ms); }, STALL.every_s * 1000);
}
const mk = () => ({ in: 0, in_bytes: 0, ok: 0, err: 0, err_codes: {}, win: [], wbytes: 0, burst_10ms_pkts: 0, burst_10ms_bytes: 0 });
const dir = { up: mk(), down: mk() }; // up: Chrome -> host, down: host -> Chrome
const buffers = []; const ports = {}; const socks = [];
function note(d, n) {
  const t = performance.now(); d.in++; d.in_bytes += n; d.win.push(t, n); d.wbytes += n;
  while (d.win.length && t - d.win[0] > 10) { d.wbytes -= d.win[1]; d.win.splice(0, 2); }
  if (d.win.length / 2 > d.burst_10ms_pkts) d.burst_10ms_pkts = d.win.length / 2;
  if (d.wbytes > d.burst_10ms_bytes) d.burst_10ms_bytes = d.wbytes;
}
const sent = (d) => (e) => { if (e) { d.err++; d.err_codes[e.code || e.message] = (d.err_codes[e.code || e.message] || 0) + 1; } else d.ok++; };
function bufs(sock, label) {
  const r = { label };
  try { sock.setRecvBufferSize(BUF); sock.setSendBufferSize(BUF); } catch (e) { r.set_error = e.message; }
  try { r.recv = sock.getRecvBufferSize(); r.send = sock.getSendBufferSize(); } catch (e) { r.read_error = e.message; }
  buffers.push(r);
}
async function relayFor(hostPort) {
  if (ports[hostPort]) return ports[hostPort];
  const inbound = dgram.createSocket('udp4'); socks.push(inbound);
  const peers = new Map();
  inbound.on('message', (msg, rinfo) => {
    const k = rinfo.address + ':' + rinfo.port;
    let out = peers.get(k);
    if (!out) {
      out = dgram.createSocket('udp4'); socks.push(out);
      out.on('message', (m) => {
        note(dir.down, m.length);
        if (stall.holding) { stall.packets_held++; stall.bytes_held += m.length; stall.held.push({ sock: inbound, msg: m, port: rinfo.port, addr: rinfo.address }); return; }
        inbound.send(m, rinfo.port, rinfo.address, sent(dir.down));
      });
      out.on('error', () => {});
      out.bind(0, () => bufs(out, 'to-host:' + hostPort));
      peers.set(k, out);
    }
    note(dir.up, msg.length);
    out.send(msg, hostPort, HOST, sent(dir.up));
  });
  inbound.on('error', () => {});
  const bindTo = (p) => new Promise((res, rej) => { inbound.once('error', rej); inbound.bind(p, '127.0.0.1', () => res(inbound.address().port)); });
  let local; try { local = await bindTo(hostPort); } catch (_) { local = await bindTo(0); }
  bufs(inbound, 'from-chrome:' + local);
  ports[hostPort] = local;
  armStall();
  return local;
}
const strip = (d) => { const o = Object.assign({}, d); delete o.win; delete o.wbytes; return o; };
parentPort.on('message', async (m) => {
  if (m.type === 'port') parentPort.postMessage({ id: m.id, value: await relayFor(m.hostPort) });
  else if (m.type === 'stats') parentPort.postMessage({ id: m.id, value: { up: strip(dir.up), down: strip(dir.down), buffers, ports, stall: STALL ? { ms: STALL.ms, every_s: STALL.every_s, stalls: stall.stalls, packets_held: stall.packets_held, bytes_held: stall.bytes_held, max_held_in_one: stall.max_held_in_one } : null } });
  else if (m.type === 'close') { if (stallTimer) clearInterval(stallTimer); for (const s of socks) { try { s.close(); } catch (_) {} } parentPort.postMessage({ id: m.id, value: true }); }
});
`;
const relay = { tcp: null, tcp_port: null, worker: null, seq: 0, waits: new Map() };
function relayCall(msg) {
  if (!relay.worker) {
    relay.worker = new Worker(RELAY_WORKER, { eval: true, workerData: { hostIp, buf: RELAY_BUF, stall: RELAY_STALL } });
    relay.worker.on('message', (m) => { const w = relay.waits.get(m.id); if (w) { relay.waits.delete(m.id); w(m.value); } });
    relay.worker.on('error', (e) => { R.client.relay && (R.client.relay.worker_error = e.message); });
  }
  const id = ++relay.seq;
  return new Promise((res) => { relay.waits.set(id, res); relay.worker.postMessage({ ...msg, id }); setTimeout(() => { if (relay.waits.delete(id)) res(null); }, 5000); });
}
// TCP (HTTPS) pipe on 127.0.0.1:<port>, or a free port when the host itself listens there.
async function startTcpRelay(port) {
  relay.tcp = net.createServer((c) => {
    const u = net.connect(port, hostIp);
    c.pipe(u); u.pipe(c);
    const end = () => { c.destroy(); u.destroy(); };
    c.on('error', end); u.on('error', end); c.on('close', end); u.on('close', end);
  });
  const listen = (p) => new Promise((res, rej) => { relay.tcp.once('error', rej); relay.tcp.listen(p, '127.0.0.1', () => res(relay.tcp.address().port)); });
  try { relay.tcp_port = await listen(port); } catch { relay.tcp_port = await listen(0); }
  return relay.tcp_port;
}
const udpRelayFor = (hostPort) => relayCall({ type: 'port', hostPort });
// Host candidates on the host's address -> loopback relay ports; every other candidate is dropped,
// so Chrome can only take the relayed pair.
async function rewriteAnswer(sdp) {
  const eol = sdp.includes('\r\n') ? '\r\n' : '\n';
  const out = [];
  for (const line of sdp.split(eol)) {
    const m = /^(a=candidate:\S+ \d+ udp \d+ )(\S+) (\d+)( typ host.*)$/i.exec(line);
    if (m && m[2] === hostIp) { out.push(`${m[1]}127.0.0.1 ${await udpRelayFor(Number(m[3]))}${m[4]}`); continue; }
    if (/^a=candidate:/i.test(line)) continue;
    out.push(line);
  }
  return out.join(eol);
}
async function stopRelay() {
  try { relay.tcp && relay.tcp.close(); } catch {}
  if (relay.worker) { const st = await relayCall({ type: 'stats' }); await relayCall({ type: 'close' }); await relay.worker.terminate(); return st; }
  return null;
}

// ---------------------------------------------------------------- CDP
async function cdp(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0; const pending = new Map(); const events = []; const hooks = [];
  ws.onmessage = (m) => {
    const d = JSON.parse(m.data);
    if (d.id && pending.has(d.id)) { const p = pending.get(d.id); pending.delete(d.id); d.error ? p.rej(new Error(JSON.stringify(d.error))) : p.res(d.result); }
    else if (d.method) { events.push(d); hooks.forEach((h) => h(d)); }
  };
  return {
    events,
    send: (method, params = {}, sessionId) => new Promise((res, rej) => { const my = ++id; pending.set(my, { res, rej }); ws.send(JSON.stringify(sessionId ? { id: my, method, params, sessionId } : { id: my, method, params })); setTimeout(() => { if (pending.delete(my)) rej(new Error('CDP timeout: ' + method)); }, 15000); }),
    on: (fn) => { hooks.push(fn); },
    close: () => ws.close(),
  };
}

// ---------------------------------------------------------------- telemetry extraction
// `status --json` → { …, telemetry: { sessions: { <join>: {…} }, live_transport_sessions } }.
// Kept whole in the document; this picks out the numbers the summary table and the verdict use.
function sessionsOf(status) {
  const t = status && (status.telemetry || status);
  const s = t && t.sessions;
  return s ? Object.entries(s).map(([k, v]) => ({ key: k, ...v })) : [];
}
function mainSession(status, joinId) {
  const all = sessionsOf(status);
  return (joinId && all.find((x) => x.key === joinId)) || all.sort((a, b) => (b.frames_encoded || 0) - (a.frames_encoded || 0))[0] || null;
}
// Our join in `status --json` → telemetry.joins[] (name MacE2E): token, joinId, display geometry.
function ourJoin(status) {
  const j = ((status && status.telemetry && status.telemetry.joins) || []).filter((x) => x.name === DEVICE_NAME);
  return j[j.length - 1] || null;
}

// Paint timing inside the video worker: every canvas paint is one drawImage on the worker's
// OffscreenCanvas, so wrapping it gives the exact interval between paints. Installed through CDP
// before the worker's first line runs; it records only while armed by the harness.
//
// The same hook also reads each encoded frame as the worker's pump reads it (by wrapping
// ReadableStreamDefaultReader.read in the worker), records `getMetadata()` — which fields this
// Chrome exposes, and `receiveTime` (reception of the frame's last packet) when it does — plus
// the host capture time the worker itself derives from the RTP stamp and the page's clock offset,
// and matches each to its paint. That splits host-capture->paint per frame into
// capture->receive (host pipeline + network, through the clock offset), receive->arrival (inside
// Chrome, before the worker sees it) and arrival->paint (decode + paint).
const PAINT_HOOK = `(() => { if (self.__sePaint) return 'present';
  const N = 1 << 15;
  const g = self.__sePaint = { n: 0, last: 0, on: false, max: 0, iv: new Float32Array(1 << 16), k: 0, start: 0,
    pend: new Map(), fields: null, sample: null, metaErr: null, reads: [], draws: [],
    seg: { c2r: new Float64Array(N), r2a: new Float64Array(N), a2p: new Float64Array(N), c2p: new Float64Array(N), c2a: new Float64Array(N) }, sk: { c2r: 0, r2a: 0, a2p: 0, c2p: 0, c2a: 0 } };
  const put = (k, v) => { if (typeof v === 'number' && isFinite(v) && g.sk[k] < N) g.seg[k][g.sk[k]++] = v; };
  const RD = self.ReadableStreamDefaultReader && ReadableStreamDefaultReader.prototype;
  if (RD && RD.read) {
    const r0 = RD.read;
    RD.read = function () {
      return r0.apply(this, arguments).then(function (res) {
        try {
          const f = res && res.value;
          if (g.on && f && typeof f.getMetadata === 'function' && typeof f.timestamp === 'number') {
            const t = performance.now();
            const md = f.getMetadata() || {};
            if (!g.fields) { g.fields = Object.keys(md); g.sample = {}; g.fields.forEach(function (k) { const v = md[k]; g.sample[k] = (typeof v === 'object' && v !== null) ? '[' + (v.constructor && v.constructor.name) + ']' : v; }); }
            let hc = null;
            try { if (typeof hostCaptureAbsMsOf === 'function') { const a = hostCaptureAbsMsOf(f.timestamp); if (a !== null) hc = a - performance.timeOrigin; } } catch (e) { g.metaErr = String(e); }
            const rt = typeof md.receiveTime === 'number' ? md.receiveTime : null;
            g.pend.set(f.timestamp >>> 0, { arr: t, recv: rt, hc: hc });
            if (g.reads.length < 24000) g.reads.push(f.timestamp >>> 0, rt === null ? -1 : rt + performance.timeOrigin, t + performance.timeOrigin);
            if (g.pend.size > 512) g.pend.delete(g.pend.keys().next().value);
          }
        } catch (e) { g.metaErr = String(e); }
        return res;
      });
    };
  }
  // A presenter that is not OffscreenCanvas 2D (a test variant) calls this right after it hands
  // the frame on, so its draw instant lands in the same timeline.
  self.__seMark = function (ts) { if (g.on && g.draws.length < 16000) g.draws.push(ts >>> 0, performance.now() + performance.timeOrigin); };
  const P = self.OffscreenCanvasRenderingContext2D && OffscreenCanvasRenderingContext2D.prototype;
  if (!P || !P.drawImage) return 'no-offscreen-2d';
  const f = P.drawImage;
  P.drawImage = function (fr) {
    if (g.spin > 0) { const s0 = performance.now(); while (performance.now() - s0 < g.spin) { /* weak-client proxy */ } }
    const t = performance.now();
    if (g.on) {
      const d = t - g.last; if (d > g.max) g.max = d; if (g.k < g.iv.length) g.iv[g.k++] = d; g.n++;
      if (fr && typeof fr.timestamp === 'number' && g.draws.length < 16000) g.draws.push(fr.timestamp >>> 0, t + performance.timeOrigin);
      try {
        const e = fr && typeof fr.timestamp === 'number' ? g.pend.get(fr.timestamp >>> 0) : null;
        if (e) {
          g.pend.delete(fr.timestamp >>> 0);
          put('a2p', t - e.arr);
          if (e.recv !== null) put('r2a', e.arr - e.recv);
          if (e.hc !== null) { put('c2p', t - e.hc); put('c2a', e.arr - e.hc); if (e.recv !== null) put('c2r', e.recv - e.hc); }
        }
      } catch (e2) { g.metaErr = String(e2); }
    }
    g.last = t; return f.apply(this, arguments);
  };
  return 'hooked'; })()`;
const PAINT_ARM = `(() => { const g = self.__sePaint; if (!g) return false; g.reads = []; g.draws = []; g.on = true; g.n = 0; g.k = 0; g.max = 0; g.last = performance.now(); g.start = g.last; g.pend.clear(); Object.keys(g.sk).forEach(function (k) { g.sk[k] = 0; }); return true; })()`;
const META_READ = `(() => { const g = self.__sePaint; if (!g) return null;
  const pct = (arr, n) => { const a = Array.from(arr.subarray(0, n)).sort((x, y) => x - y); const q = (p) => (a.length ? Math.round(a[Math.min(a.length - 1, Math.floor(p * a.length))] * 100) / 100 : null); return { n: a.length, p50: q(0.5), p90: q(0.9), p99: q(0.99), min: a.length ? Math.round(a[0] * 100) / 100 : null }; };
  const o = { fields: g.fields, sample: g.sample, error: g.metaErr };
  Object.keys(g.sk).forEach(function (k) { o[k] = pct(g.seg[k], g.sk[k]); });
  return o; })()`;
const PAINT_READ = `(() => { const g = self.__sePaint; if (!g || !g.on) return null; const t = performance.now(); g.on = false;
  const tail = t - g.last; const a = Array.from(g.iv.subarray(0, g.k)).sort((x, y) => x - y); const q = (p) => (a.length ? a[Math.min(a.length - 1, Math.floor(p * a.length))] : null);
  const r2 = (x) => (x == null ? null : Math.round(x * 100) / 100);
  return { paints: g.n, window_ms: r2(t - g.start), fps: r2(g.n / ((t - g.start) / 1000)), interval_p50_ms: r2(q(0.5)), interval_p90_ms: r2(q(0.9)), interval_p99_ms: r2(q(0.99)),
    longest_gap_ms: r2(Math.max(g.max, tail)), trailing_gap_ms: r2(tail), gaps_over_50ms: a.filter((x) => x > 50).length, gaps_over_100ms: a.filter((x) => x > 100).length, gaps_over_250ms: a.filter((x) => x > 250).length }; })()`;

// ---------------------------------------------------------------- --tick analysis
async function collectTick(ev, inWorkers) {
  const page = await ev('window.__seTick ? (window.__seTick.on = false, { vs: window.__seTick.vs, rvfc: window.__seTick.rvfc, mdraw: window.__seTick.mdraw, hooked: window.__seTick.hooked, desync: window.__seTick.desyncApplied, err: window.__seTick.err || null, jbt: window.__seJbt || null, visibility: document.visibilityState }) : null').catch((e) => ({ error: e.message }));
  const ws = (await inWorkers('self.__sePaint ? { reads: self.__sePaint.reads, draws: self.__sePaint.draws } : null')).map((x) => x.value).filter((v) => v && v.reads).sort((a, b) => b.reads.length - a.reads.length)[0] || { reads: [], draws: [] };
  const pct = (a) => { const s = a.filter((x) => isFinite(x)).sort((x, y) => x - y); const q = (p) => (s.length ? +s[Math.min(s.length - 1, Math.floor(p * s.length))].toFixed(2) : null); return { n: s.length, p10: q(0.1), p50: q(0.5), p90: q(0.9), p99: q(0.99), max: s.length ? +s[s.length - 1].toFixed(2) : null }; };
  const recv = new Map(), read = new Map();
  for (let i = 0; i + 2 < ws.reads.length; i += 3) { if (ws.reads[i + 1] > 0) recv.set(ws.reads[i], ws.reads[i + 1]); read.set(ws.reads[i], ws.reads[i + 2]); }
  const draw = new Map();
  for (let i = 0; i + 1 < ws.draws.length; i += 2) if (!draw.has(ws.draws[i])) draw.set(ws.draws[i], ws.draws[i + 1]);
  const mdraws = (page && page.mdraw) || [];
  for (let i = 0; i + 1 < mdraws.length; i += 2) if (!draw.has(mdraws[i])) draw.set(mdraws[i], mdraws[i + 1]);
  const vs = ((page && page.vs) || []).slice().sort((a, b) => a - b);
  const nextV = (t) => { let lo = 0, hi = vs.length; while (lo < hi) { const m = (lo + hi) >> 1; if (vs[m] <= t) lo = m + 1; else hi = m; } return lo < vs.length ? vs[lo] : null; };
  const prevV = (t) => { let lo = 0, hi = vs.length; while (lo < hi) { const m = (lo + hi) >> 1; if (vs[m] <= t) lo = m + 1; else hi = m; } return lo > 0 ? vs[lo - 1] : null; };
  const rv = new Map();
  for (const r of (page && page.rvfc) || []) if (!rv.has(r[0])) rv.set(r[0], r);
  const r2read = [], r2draw = [], r2disp = [], r2dispRvfc = [], readPhase = [], recvPhase = [], drawPhase = [];
  for (const [ts, rcv] of recv) {
    const rd = read.get(ts); if (rd != null) { r2read.push(rd - rcv); const pv = prevV(rd); if (pv != null) readPhase.push(rd - pv); }
    const pr = prevV(rcv); if (pr != null) recvPhase.push(rcv - pr);
    const dr = draw.get(ts);
    if (dr != null) { r2draw.push(dr - rcv); const nv = nextV(dr); if (nv != null) r2disp.push(nv - rcv); const pd = prevV(dr); if (pd != null) drawPhase.push(dr - pd); }
    const v = rv.get(ts); if (v) r2dispRvfc.push(v[1] - rcv);
  }
  // rVFC on a <video> fed by WebRTC itself carries its own receiveTime.
  const rvfcOwn = [];
  for (const r of (page && page.rvfc) || []) if (r[3] > 0) rvfcOwn.push(r[1] - r[3]);
  const iv = []; for (let i = 1; i < vs.length; i++) iv.push(vs[i] - vs[i - 1]);
  // Is the worker's read locked to a periodic clock? Mean resultant length of the read instants
  // folded onto each candidate period: ~1 = every read at the same phase of that clock, ~0 = none.
  const readsAbs = [...read.values()];
  const lockTo = (P) => { if (!readsAbs.length) return null; let c = 0, sn = 0; for (const t of readsAbs) { const a = (2 * Math.PI * (t % P)) / P; c += Math.cos(a); sn += Math.sin(a); } return +(Math.hypot(c, sn) / readsAbs.length).toFixed(3); };
  const recvAbs = [...recv.values()];
  const lockRecv = (P) => { if (!recvAbs.length) return null; let c = 0, sn = 0; for (const t of recvAbs) { const a = (2 * Math.PI * (t % P)) / P; c += Math.cos(a); sn += Math.sin(a); } return +(Math.hypot(c, sn) / recvAbs.length).toFixed(3); };
  const periods = [1000 / 64, 1000 / 60, 10, 1000 / 120, 5];
  const within = (a, lo, hi) => (a.length ? +(a.filter((x) => x >= lo && x < hi).length / a.length).toFixed(3) : null);
  return {
    frames: { read: read.size, with_receive_time: recv.size, drawn: draw.size, rvfc: rv.size, raf: vs.length },
    page: page && { hooked_videos: page.hooked, desync_applied: page.desync, error: page.err, jbt: page.jbt, visibility: page.visibility, error2: page.error },
    raf_interval_ms: pct(iv),
    receive_to_read_ms: pct(r2read),
    receive_to_draw_ms: pct(r2draw),
    receive_to_next_vsync_after_draw_ms: pct(r2disp),
    receive_to_expected_display_rvfc_ms: pct(r2dispRvfc),
    rvfc_own_receive_to_expected_display_ms: pct(rvfcOwn),
    // Where in the refresh interval things happen: time since the last rAF timestamp. A read
    // locked to the page's frame clock shows up as a narrow band here.
    phase_lock: Object.fromEntries(periods.map((P) => [P.toFixed(3) + 'ms', { read: lockTo(P), receive: lockRecv(P) }])),
    phase_after_vsync_ms: { receive: pct(recvPhase), read: pct(readPhase), draw: pct(drawPhase), read_within_0_3ms: within(readPhase, 0, 3), receive_within_0_3ms: within(recvPhase, 0, 3) },
  };
}

// ---------------------------------------------------------------- main
let chrome = null, chromeExited = true, profile = null, hostStarted = false, dataDir = null, mover = null;
let remoteChrome = null; // { profile, port, tunnel } when --client
let cleaned = false;
async function cleanup(reason) {
  if (cleaned) return; cleaned = true;
  step('cleanup', { reason });
  if (chrome && !chromeExited) { try { chrome.kill('SIGTERM'); } catch {} for (let i = 0; i < 30 && !chromeExited; i++) await sleep(100); if (!chromeExited) try { chrome.kill('SIGKILL'); } catch {} }
  if (profile) fs.rmSync(profile, { recursive: true, force: true });
  if (remoteChrome) {
    const rc = remoteChrome; remoteChrome = null;
    const k = await shA('ssh', ['-o', 'BatchMode=yes', CLIENT, `pkill -f '${rc.profile}'; sleep 1; pkill -9 -f '${rc.profile}'; rm -rf '${rc.profile}'; echo LEFT=$(pgrep -f '${rc.profile}' | tr '\\n' ' ')`]);
    R.client.remote_cleanup = { left: (kv(k.out).LEFT || '').trim() || null };
    if (R.client.remote_cleanup.left) R.failures.push('Chrome left running on the client Mac: ' + R.client.remote_cleanup.left);
    try { rc.tunnel.kill('SIGTERM'); } catch {}
  }
  if (R.client.relay && R.client.relay.used) { const st = await stopRelay(); Object.assign(R.client.relay, { tcp_port: relay.tcp_port, final: st }); if (RELAY_STALL && !(st && st.stall && st.stall.stalls > 0)) R.failures.push('--relay-stall: the relay held nothing'); }
  if (hostStarted && MOTION_ARG && MOTION_ARG.kind === 'window') {
    const w = kv((await hostRun(['killwin'], { timeoutMs: 15_000 })).out).KILLED_WINDOWS;
    R.motion.windows_killed = Number(w);
    if (Number(w) > 0) R.failures.push(`${w} motion window process(es) still up at the end of the run; killed`);
  }
  if (hostStarted) {
    const r = await hostRun(['stop', BIN, dataDir, String(HTTP_PORT), String(HTTPS_PORT)], { timeoutMs: 45_000 });
    const s = kv(r.out);
    R.host.stop = { stop_exit: s.STOP_EXIT, stop_ms: Number(s.STOP_MS), stopped_by: s.STOPPED_BY, left_pid: s.LEFT_PID || null, left_listeners: (s.LEFT_LISTENERS || '').trim() || null, left_procs: (s.LEFT_PROCS || '').trim() || null, stop_output: s.STOP_OUT };
    // Keep the host log (OTP redacted), then remove the isolated data dir.
    const log = LOCAL ? sh('cat', [path.join(dataDir, 'serve.log')]).out : ssh(`cat '${dataDir}/serve.log'`).out;
    const red = log.split(OTP).join('######');
    const logPath = OUT.replace(/\.json$/, '.serve.log');
    fs.writeFileSync(logPath, red);
    R.host.serve_log = logPath;
    R.host.log_highlights = red.split('\n').filter((l) => /\[permissions\]|capture|encoder|VideoToolbox|rate tier|rate_tier|hold|perf|synthetic|SE_TEST|error|warn|panic|fps=/i.test(l) && !/^\s*$/.test(l)).slice(-80);
    if (R.host.stop.stopped_by === 'SIGKILL') { const hs = LOCAL ? sh('cat', [path.join(dataDir, 'hang.sample')]).out : ssh(`cat '${dataDir}/hang.sample'`).out; if (hs) fs.writeFileSync(OUT.replace(/\.json$/, '.hang.sample.txt'), hs); }
    if (LOCAL) fs.rmSync(dataDir, { recursive: true, force: true }); else ssh(`rm -rf '${dataDir}'`);
    const p = await hostRun(['probe', mover || '']);
    const after = displays(kv(p.out).DISPLAYS);
    R.screen_locked_end = kv(p.out).LOCKED === '1';
    R.host.attach_timeline = red.split('\n').filter((l) => /\[display\] attach timeline|did not attach|\[display\].*(retry|recreat)/i.test(l)).slice(0, 40);
    R.host.governor_lines = red.split('\n').filter((l) => /governor/i.test(l)).slice(0, 40);
    if (R.screen_locked_end && !R.screen_locked) R.failures.push('the host screen locked during the run');
    if (R.host.power_source) R.host.power_source.end = kv(p.out).POWER_SOURCE || null;
    R.host.tune_lines = red.split('\n').filter((l) => /\[tune\]|performance hold|\[clock\]|qp_governor|hold.?push.*(suspend|guard)/i.test(l)).slice(0, 40);
    R.host.displays_after_stop = after;
    if (R.host.displays_before && after) R.host.virtual_display_removed = after.length === R.host.displays_before.length;
    if (R.host.stop.stopped_by !== 'stop') {
      R.failures.push(`host did not stop within 20 s; ${R.host.stop.stopped_by}`);
    }
    if (R.host.stop.left_pid || R.host.stop.left_listeners || R.host.stop.left_procs) R.failures.push('host left a process or listener behind');
    if (R.host.virtual_display_removed === false) R.failures.push('virtual display still present after stop');
  }
}
const displays = (s) => { try { return JSON.parse(s); } catch { return null; } };

async function main() {
  if (!IN_LOCK) {
    if (!LOCAL) await waitRemoteFree();
    fs.mkdirSync(path.dirname(LOCK), { recursive: true });
    console.log(`[${now()}] waiting for ${LOCK} …`);
    const r = spawnSync('lockf', ['-k', '-t', '7200', LOCK, process.execPath, fileURLToPath(import.meta.url), ...argv],
      { stdio: 'inherit', env: { ...process.env, E2E_RUNMAC_IN_LOCK: '1' } });
    process.exit(r.status == null ? 1 : r.status);
  }
  if (!LOCAL) await waitRemoteFree();
  // Armed after the wait for a free remote host, so only the test itself is on the clock.
  const deadline = setTimeout(() => { R.failures.push(`deadline ${DEADLINE_MS / 1000}s exceeded`); finish(1, 'deadline'); }, LOCAL ? DEADLINE_MS - (Date.now() - T_START) : DEADLINE_MS);
  deadline.unref();
  mover = LOCAL ? ensureLocalMover() : await ensureRemoteMover();
  const motionWin = MOTION_ARG && MOTION_ARG.kind === 'window' ? (LOCAL ? ensureLocalMotionWin() : await ensureRemoteBin(ensureLocalMotionWin(), 'motion-window')) : null;

  // ---- host preflight + start
  const pre = kv((await hostRun(['probe', mover])).out);
  R.host.os = pre.OS;
  R.host.power_source = { start: pre.POWER_SOURCE || null };
  R.host.displays_before = displays(pre.DISPLAYS);
  R.screen_locked = pre.LOCKED === '1';
  if (!LOCAL && (pre.BENCH_PROCS || '').trim()) throw new Error('a ScreenExtend-bench process appeared on the host: ' + pre.BENCH_PROCS);
  dataDir = LOCAL ? fs.mkdtempSync(path.join(os.tmpdir(), 'se-e2e-host-'))
    : ssh('mktemp -d /tmp/se-e2e-host.XXXXXX').out.trim();
  if (!dataDir) throw new Error('could not create the host data dir');
  R.host.data_dir = dataDir;
  hostStarted = true; // from here on, cleanup always stops it
  const hostEnv = { SCREENEXTEND_SESSION_ID: SID, SCREENEXTEND_OTP: OTP, ...(SYNTHETIC ? { SE_TEST_HOOKS: '1', SE_TEST_SOURCE: SYNTHETIC } : {}), ...HOST_ENV };
  const st = await hostRun(['start', BIN, dataDir, String(HTTP_PORT), String(HTTPS_PORT)], { env: hostEnv, timeoutMs: 60_000 });
  const s = kv(st.out);
  R.host.process_env_se = (s.HOST_ENV || '').trim() || null;
  const seen = new Set((R.host.process_env_se || '').split(/\s+/).filter(Boolean));
  for (const [k, v] of Object.entries(hostEnv)) if (/^SE_/.test(k) && !seen.has(`${k}=${v}`)) R.failures.push(`${k}=${v} did not reach the host process`);
  R.host.pid = Number(s.PID) || null;
  R.host.startup_ms = Number(s.STARTUP_MS);
  if (s.READY !== '1') { R.host.start_output = st.out.split(OTP).join('######'); throw new Error('host did not start: ' + (s.ERROR || st.err.trim())); }
  step('host-ready', { pid: R.host.pid, startup_ms: R.host.startup_ms, locked: s.LOCKED_AT_START === '1' });

  // ---- Chrome: fresh profile, nothing of the user's
  const chromeArgs = (port, prof) => [
    ...(flag('headed') && !CLIENT ? [] : ['--headless=new']), `--remote-debugging-port=${port}`, `--user-data-dir=${prof}`,
    '--ignore-certificate-errors', '--autoplay-policy=no-user-gesture-required', '--use-fake-device-for-media-stream',
    '--use-fake-ui-for-media-stream', '--mute-audio', `--window-size=${SIZE_W},${SIZE_H}`, '--no-first-run', '--no-default-browser-check',
    '--disable-background-networking', '--disable-component-update', '--disable-sync', '--metrics-recording-only',
    `--disable-features=${['MediaRouter', 'OptimizationHints', 'Translate', ...CHROME_FLAGS.filter((f) => f.startsWith('--disable-features=')).map((f) => f.split('=')[1])].join(',')}`,
    ...CHROME_FLAGS.filter((f) => !f.startsWith('--disable-features=')),
    ...(SOFTWARE_DECODE ? ['--disable-accelerated-video-decode'] : []), 'about:blank',
  ];
  let port, wsFix = (u) => u;
  if (CLIENT) {
    const busy = sshC('pgrep -f "ScreenExtend-bench[^ ]* bench" | tr "\\n" " "').out.trim();
    if (busy) throw new Error('the client Mac is busy (ScreenExtend-bench pids ' + busy + ')');
    // Chrome on the client Mac, headless, its own temp profile there; CDP over an SSH forward.
    const info = sshC(`sw_vers -productVersion; uname -m; '${REMOTE_CHROME}' --version`).out.trim().split('\n');
    R.client.machine = `${CLIENT.split('@').pop()} ${info[1] || ''} macOS ${info[0] || ''}`;
    const rprof = sshC('mktemp -d /tmp/se-e2e-chrome.XXXXXX').out.trim();
    if (!rprof) throw new Error('could not make a Chrome profile dir on the client');
    const rport = 9300 + crypto.randomInt(0, 600);
    const q = (x) => `'${x}'`;
    const launch = sshC(`nohup ${q(REMOTE_CHROME)} ${chromeArgs(rport, rprof).map(q).join(' ')} > ${q(rprof + '/se-chrome.log')} 2>&1 < /dev/null & echo PID=$!`);
    port = await freePort();
    const tunnel = spawn('ssh', ['-N', '-o', 'BatchMode=yes', '-o', 'ExitOnForwardFailure=yes', '-L', `${port}:127.0.0.1:${rport}`, CLIENT], { stdio: 'ignore' });
    remoteChrome = { profile: rprof, port: rport, pid: kv(launch.out).PID, tunnel };
    chromeExited = false;
    wsFix = (u) => u.replace(/^ws:\/\/[^/]+/, `ws://127.0.0.1:${port}`);
  } else {
    profile = fs.mkdtempSync(path.join(os.tmpdir(), 'se-e2e-chrome-'));
    port = await freePort();
    chrome = spawn(CHROME, chromeArgs(port, profile), { stdio: 'ignore' });
    chromeExited = false; chrome.on('exit', () => { chromeExited = true; });
  }
  let ver = null;
  for (let i = 0; i < 100 && !ver; i++) { try { ver = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json(); } catch { await sleep(200); } }
  if (!ver) throw new Error('Chrome debugging port never answered' + (CLIENT ? ' (client Mac)' : ''));
  R.client.browser = ver.Browser;
  const browser = await cdp(wsFix(ver.webSocketDebuggerUrl));
  const tgt = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: 'PUT' })).json();
  const page = await cdp(wsFix(tgt.webSocketDebuggerUrl));
  if (CPU_THROTTLE) R.client.cpu_throttle = { rate: CPU_THROTTLE, page: await page.send('Emulation.setCPUThrottlingRate', { rate: CPU_THROTTLE }).then(() => 'ok', (e) => 'error ' + e.message), workers: [] };
  page.on((d) => { if (d.method === 'Page.javascriptDialogOpening') page.send('Page.handleJavaScriptDialog', { accept: true }).catch(() => {}); });
  await page.send('Runtime.enable'); await page.send('Page.enable'); await page.send('Log.enable');
  const ev = async (expr, gesture = false) => {
    const r = await page.send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true, userGesture: gesture });
    if (r.exceptionDetails) throw new Error('page threw: ' + JSON.stringify(r.exceptionDetails.exception || r.exceptionDetails.text));
    return r.result.value;
  };
  // Observation only: keep the peer connections where the harness can read getStats().
  await page.send('Page.addScriptToEvaluateOnNewDocument', { source: `(() => {
    const P = window.RTCPeerConnection; if (!P) return; window.__pcs = [];
    window.RTCPeerConnection = function (c) { const pc = new P(c); window.__pcs.push(pc); return pc; };
    window.RTCPeerConnection.prototype = P.prototype;
    for (const k of Object.getOwnPropertyNames(P)) { try { if (!(k in window.RTCPeerConnection)) window.RTCPeerConnection[k] = P[k]; } catch (_) {} }
  })();` });
  if (NO_SCRIPT_TRANSFORM) await page.send('Page.addScriptToEvaluateOnNewDocument', { source: 'try { delete window.RTCRtpScriptTransform; } catch (_) {}' });
  if (JBT0) {
    // Set in the same task as addTransceiver('video') returns, so nothing yields before the
    // page installs its transform (which must also happen in that task).
    await page.send('Page.addScriptToEvaluateOnNewDocument', { source: `(() => {
      const A = RTCPeerConnection.prototype.addTransceiver;
      window.__seJbt = [];
      RTCPeerConnection.prototype.addTransceiver = function (k) {
        const t = A.apply(this, arguments);
        if (k === 'video') { try { t.receiver.jitterBufferTarget = 0; window.__seJbt.push('jitterBufferTarget=' + t.receiver.jitterBufferTarget); } catch (e) { window.__seJbt.push('jbt ' + e); }
          try { t.receiver.playoutDelayHint = 0; window.__seJbt.push('playoutDelayHint=' + t.receiver.playoutDelayHint); } catch (e) { window.__seJbt.push('pdh ' + e); } }
        return t;
      };
    })();` });
  }
  if (DC_PING) {
    await page.send('Page.addScriptToEvaluateOnNewDocument', { source: `(() => {
      const C = RTCPeerConnection.prototype.createDataChannel;
      const D = window.__seDc = { ch: null, sent: new Map(), rtt: [], pong_abs: [], others: 0, done: false };
      RTCPeerConnection.prototype.createDataChannel = function (label) {
        const ch = C.apply(this, arguments);
        if (label === 'reliable') {
          D.ch = ch;
          ch.addEventListener('message', function (m) {
            const t = performance.now();
            const b = m.data; if (!(b instanceof ArrayBuffer) || b.byteLength < 9) return;
            const v = new DataView(b); if (v.getUint8(0) !== 0x61) return;
            const st = Number(v.getBigUint64(1, true));
            if (D.sent.has(st)) { D.rtt.push(t - D.sent.get(st)); D.pong_abs.push(performance.timeOrigin + t); D.sent.delete(st); } else D.others++;
          });
        }
        return ch;
      };
      D.run = function (n) {
        let i = 0;
        const one = function () {
          if (i++ >= n) { D.done = true; return; }
          if (D.ch && D.ch.readyState === 'open') {
            const t = performance.now(); const st = Math.round(t * 1e6);
            const buf = new ArrayBuffer(9); const v = new DataView(buf); v.setUint8(0, 0x60); v.setBigUint64(1, BigInt(st), true);
            D.sent.set(st, t); D.ch.send(buf);
          }
          setTimeout(one, 20 + Math.random() * 100);
        };
        one();
      };
    })();` });
  }
  if (TICK) {
    // Page side of the per-frame timeline: every rAF timestamp (the frame's vsync-aligned begin
    // time), rVFC on any <video> the page shows, and a main-thread presenter for a test worker
    // variant that posts its decoded frames here ({type:'sevf'}) instead of painting them.
    await page.send('Page.addScriptToEvaluateOnNewDocument', { source: `(() => {
      const T = window.__seTick = { on: false, vs: [], rvfc: [], mdraw: [], hooked: 0 };
      const raf = function (ts) { if (T.on && T.vs.length < 40000) T.vs.push(performance.timeOrigin + ts); requestAnimationFrame(raf); };
      requestAnimationFrame(raf);
      setInterval(function () {
        document.querySelectorAll('video').forEach(function (v) {
          if (v.__seRvfc || typeof v.requestVideoFrameCallback !== 'function') return;
          v.__seRvfc = true; T.hooked++;
          const cb = function (now, md) {
            if (T.on && T.rvfc.length < 40000) T.rvfc.push([Math.round((md.mediaTime || 0) * 1e6), performance.timeOrigin + md.expectedDisplayTime, performance.timeOrigin + md.presentationTime, md.receiveTime != null ? performance.timeOrigin + md.receiveTime : -1, md.rtpTimestamp != null ? md.rtpTimestamp : -1]);
            v.requestVideoFrameCallback(cb);
          };
          v.requestVideoFrameCallback(cb);
        });
      }, 250);
      let cv = null, cx = null;
      const W = window.Worker;
      window.Worker = function (u, o) {
        const w = new W(u, o);
        w.addEventListener('message', function (m) {
          const d = m.data;
          if (!d || d.type !== 'sevf' || !d.frame) return;
          const f = d.frame;
          try {
            if (!cv) {
              cv = document.createElement('canvas');
              cv.style.cssText = 'position:fixed;left:0;top:0;width:100vw;height:100vh;z-index:2147483647;background:#000';
              document.body.appendChild(cv);
              cx = cv.getContext('2d', { alpha: false, desynchronized: !!d.desync });
              T.desyncApplied = cx && cx.getContextAttributes ? !!cx.getContextAttributes().desynchronized : null;
            }
            if (cv.width !== f.displayWidth) { cv.width = f.displayWidth; cv.height = f.displayHeight; }
            cx.drawImage(f, 0, 0);
            if (T.on && T.mdraw.length < 40000) T.mdraw.push(d.ts >>> 0, performance.timeOrigin + performance.now());
          } catch (e) { T.err = String(e); }
          try { f.close(); } catch (_) {}
        });
        return w;
      };
      window.Worker.prototype = W.prototype;
    })();` });
  }
  // The page sizes its display from screen.width/height and devicePixelRatio: pin all three.
  await page.send('Emulation.setDeviceMetricsOverride', { width: SIZE_W, height: SIZE_H, deviceScaleFactor: 1, mobile: false, screenWidth: SIZE_W, screenHeight: SIZE_H });
  // Workers start paused so the paint hook is in place before the video worker's first line.
  // The video worker may be started from a blob: URL, so every dedicated worker gets the hook;
  // only the one that paints ever records anything.
  const workerSessions = new Map(); // sessionId -> { url, hook }
  R.client.paint_hook = null;
  page.on((d) => {
    if (d.method === 'Target.detachedFromTarget') { workerSessions.delete(d.params.sessionId); return; }
    if (d.method !== 'Target.attachedToTarget') return;
    const sid = d.params.sessionId, info = d.params.targetInfo || {};
    (async () => {
      if (info.type === 'worker') {
        const r = await page.send('Runtime.evaluate', { expression: PAINT_HOOK, returnByValue: true }, sid).catch((e) => ({ result: { value: 'error ' + e.message } }));
        workerSessions.set(sid, { url: String(info.url || '').slice(0, 80), hook: r.result && r.result.value });
        if (WORKER_SPIN) await page.send('Runtime.evaluate', { expression: `self.__sePaint && (self.__sePaint.spin = ${WORKER_SPIN})` }, sid).catch(() => {});
        if (CPU_THROTTLE) R.client.cpu_throttle.workers.push(await page.send('Emulation.setCPUThrottlingRate', { rate: CPU_THROTTLE }, sid).then(() => 'ok', (e) => 'error ' + e.message));
      }
    })().finally(() => page.send('Runtime.runIfWaitingForDebugger', {}, sid).catch(() => {}));
  });
  const inWorkers = async (expr) => {
    const out = [];
    for (const [sid, w] of workerSessions) out.push({ ...w, value: await page.send('Runtime.evaluate', { expression: expr, returnByValue: true }, sid).then((r) => r.result.value, (e) => ({ error: e.message })) });
    return out;
  };
  await page.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: true, flatten: true });
  const gpu = await browser.send('SystemInfo.getInfo').catch(() => null);
  R.client.gpu = gpu && gpu.gpu ? { devices: (gpu.gpu.devices || []).map((d) => d.deviceString || `${d.vendorId}:${d.deviceId}`), video_decode: (gpu.gpu.featureStatus || {}).video_decode } : null;

  // Direct first (unless --relay on); a Local Network refusal falls back to the loopback relay.
  const navErrors = [];
  page.on((d) => { if (d.method === 'Network.loadingFailed' && d.params.type === 'Document') navErrors.push(d.params.errorText); });
  await page.send('Network.enable');
  const navigate = async (origin, tries) => {
    const r = await page.send('Page.navigate', { url: `${origin}/?id=${SID}&name=${encodeURIComponent(DEVICE_NAME)}&otp=${OTP}&fs=0${URL_PARAMS.map((x) => '&' + x).join('')}` });
    if (r.errorText) navErrors.push(r.errorText);
    for (let i = 0; i < tries; i++) { if (await ev('document.readyState === "complete" && !!document.getElementById("joinForm")').catch(() => false)) return true; if (navErrors.length) return false; await sleep(200); }
    return false;
  };
  // One Fetch handler for both uses: client-file overrides (request stage) and the relay's WHEP
  // answer rewrite (response stage). Fetch.enable replaces its pattern list, so it is rebuilt.
  let relayFetch = false;
  const enableFetch = () => page.send('Fetch.enable', { patterns: [
    ...OVERRIDES.map((o) => ({ urlPattern: `*/${o.name}*`, requestStage: 'Request' })),
    ...(relayFetch ? [{ urlPattern: '*/whep*', requestStage: 'Response' }] : []),
  ] });
  R.client.overrides = OVERRIDES.map((o) => ({ name: o.name, sha256: o.sha256, bytes: o.bytes, served: 0, urls: [] }));
  page.on((d) => {
    if (d.method !== 'Fetch.requestPaused' || d.params.responseStatusCode !== undefined) return;
    const q = d.params;
    const u = new URL(q.request.url);
    const i = OVERRIDES.findIndex((o) => u.pathname === '/' + o.name);
    if (i < 0) { page.send('Fetch.continueRequest', { requestId: q.requestId }).catch(() => {}); return; }
    const o = OVERRIDES[i];
    R.client.overrides[i].served++; R.client.overrides[i].urls.push(u.pathname);
    page.send('Fetch.fulfillRequest', { requestId: q.requestId, responseCode: 200, responseHeaders: [
      { name: 'Content-Type', value: 'text/javascript; charset=utf-8' }, { name: 'Cache-Control', value: 'no-store' },
      { name: 'Cross-Origin-Resource-Policy', value: 'same-origin' }, { name: 'Cross-Origin-Embedder-Policy', value: 'require-corp' },
    ], body: o.body.toString('base64') }).catch((e) => { R.client.overrides[i].error = e.message; });
  });
  if (OVERRIDES.length) await enableFetch();
  let ORIGIN = `https://${hostIp}:${HTTPS_PORT}`;
  let tNav = Date.now();
  let loaded = RELAY === 'on' ? false : await navigate(ORIGIN, 75);
  R.client.relay = { mode: RELAY, used: false };
  if ((!loaded && RELAY !== 'off' && !LOCAL) || RELAY === 'on') {
    R.client.relay = { mode: RELAY, used: true, reason: RELAY === 'on' ? 'forced by --relay on' : `Chrome could not load the host's LAN address directly (${navErrors.join(', ') || 'timeout'}); macOS Local Network privacy has not been granted to Google Chrome on this Mac` };
    step('relay-on', R.client.relay);
    const tcpPort = await startTcpRelay(HTTPS_PORT);
    relayFetch = true;
    await enableFetch();
    page.on((d) => {
      if (d.method !== 'Fetch.requestPaused' || d.params.responseStatusCode === undefined) return;
      const q = d.params;
      (async () => {
        if (q.request.method !== 'POST' || !(q.responseStatusCode >= 200 && q.responseStatusCode < 300)) return page.send('Fetch.continueRequest', { requestId: q.requestId });
        const body = await page.send('Fetch.getResponseBody', { requestId: q.requestId });
        const text = body.base64Encoded ? Buffer.from(body.body, 'base64').toString('utf8') : body.body;
        const sdp = await rewriteAnswer(text);
        const headers = (q.responseHeaders || []).filter((h) => !/^content-length$/i.test(h.name));
        await page.send('Fetch.fulfillRequest', { requestId: q.requestId, responseCode: q.responseStatusCode, responseHeaders: headers, body: Buffer.from(sdp).toString('base64') });
      })().catch((e) => { R.client.relay.error = e.message; page.send('Fetch.continueRequest', { requestId: q.requestId }).catch(() => {}); });
    });
    navErrors.length = 0;
    ORIGIN = `https://127.0.0.1:${tcpPort}`;
    tNav = Date.now();
    loaded = await navigate(ORIGIN, 75);
  }
  if (!loaded) throw new Error('join page never loaded from ' + ORIGIN + (navErrors.length ? ` (${navErrors.join(', ')})` : ''));
  await browser.send('Browser.grantPermissions', { origin: ORIGIN, permissions: ['audioCapture', 'wakeLockScreen', 'speakerSelection'] }).catch(() => {});
  step('page-loaded', { ms: Date.now() - tNav, secure: await ev('isSecureContext'), coi: await ev('crossOriginIsolated'), screen: await ev('screen.width + "x" + screen.height + "@" + devicePixelRatio') });

  const modals = [];
  const sweep = async () => {
    const hit = await ev(`(() => {
      const shown = (id) => { const m = document.getElementById(id); if (!m) return false; const s = getComputedStyle(m); return s.display !== 'none' && s.visibility !== 'hidden' && (m.classList.contains('show') || m.style.display === 'flex'); };
      if (shown('sameDeviceModal')) { document.getElementById('sameDeviceDismiss').click(); return 'sameDevice'; }
      if (shown('compatModal')) { document.getElementById('compatContinue').click(); return 'compat'; }
      if (shown('permModal')) { document.getElementById('permContinue').click(); return 'micCard'; }
      const h = document.getElementById('dismiss'); if (h && h.offsetParent) { h.click(); return 'httpsUpsell'; }
      return null; })()`, true).catch(() => null);
    if (hit) modals.push(hit);
  };
  for (let i = 0; i < 6; i++) { await sweep(); await sleep(150); }
  if (!(await ev('!document.getElementById("connect").disabled'))) throw new Error('Connect stayed disabled: ' + await ev('document.getElementById("errorMessage").textContent'));

  // ---- join → first paint
  const tJoin = Date.now();
  await ev('document.getElementById("joinForm").requestSubmit()', true);
  let st0 = null;
  for (let i = 0; i < 150; i++) {
    await sweep();
    st0 = await ev('window.SEVideo ? SEVideo.getStats() : null').catch(() => null);
    if (st0 && st0.framesPainted > 0) break;
    await sleep(200);
  }
  const pageErr = await ev('document.getElementById("errorMessage").textContent').catch(() => '');
  R.join = { ms_submit_to_first_paint: st0 && st0.framesPainted > 0 ? Date.now() - tJoin : null, ms_navigate_to_first_paint: st0 && st0.framesPainted > 0 ? Date.now() - tNav : null, modals_dismissed: modals, page_error: pageErr || null };
  if (!st0 || !(st0.framesPainted > 0)) { R.failures.push('no frame painted within 30 s of the join'); throw new Error('join failed / nothing painted; page error: ' + JSON.stringify(pageErr)); }
  step('first-paint', R.join);

  const pcStats = () => ev(`(async () => {
    const pc = (window.__pcs || []).slice(-1)[0]; if (!pc) return null; const r = await pc.getStats(); const o = { pc: pc.connectionState };
    const cs = {}; r.forEach((s) => { if (s.type === 'codec') cs[s.id] = s.mimeType + ' ' + (s.sdpFmtpLine || ''); });
    r.forEach((s) => {
      if (s.type === 'inbound-rtp' && s.kind === 'video') Object.assign(o, { codec: cs[s.codecId], decoder: s.decoderImplementation, power_efficient: s.powerEfficientDecoder, packets: s.packetsReceived, lost: s.packetsLost, bytes: s.bytesReceived, frames_received: s.framesReceived, frames_decoded: s.framesDecoded, keyframes_decoded: s.keyFramesDecoded, frames_dropped: s.framesDropped, fps: s.framesPerSecond, width: s.frameWidth, height: s.frameHeight, jitter_ms: s.jitter * 1000, pli: s.pliCount, fir: s.firCount, nack: s.nackCount });
      if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') Object.assign(o, { ice_rtt_ms: s.currentRoundTripTime * 1000 });
    });
    return o; })()`).catch(() => null);

  // ---- settle, then the measured window
  await sleep(2000);
  const after = kv((await hostRun(['probe', mover])).out);
  const before = R.host.displays_before || [];
  const nowDisplays = displays(after.DISPLAYS) || [];
  const vd = nowDisplays.find((d) => !before.some((b) => b.id === d.id)) || null;
  R.host.virtual_display = vd;
  // Geometry evidence ~1-2 s into the session: CGDisplayBounds of every active display and any
  // pair of rectangles that overlap (a display that kept an old origin after a resize sits on top
  // of another one, and its content is then not where its bounds say).
  const rectsOverlap = (a, b) => a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
  const actives = nowDisplays.filter((x) => x.active);
  R.host.displays_in_session = actives.map((x) => ({ id: x.id, x: x.x, y: x.y, w: x.w, h: x.h, main: x.main, builtin: x.builtin }));
  R.host.display_overlaps = [];
  for (let i = 0; i < actives.length; i++) for (let j = i + 1; j < actives.length; j++) if (rectsOverlap(actives[i], actives[j])) R.host.display_overlaps.push([actives[i].id, actives[j].id]);
  R.screen_locked = R.screen_locked || after.LOCKED === '1';
  const serveLog = async (from = 0) => (await shA(...(LOCAL ? ['tail', ['-c', `+${from + 1}`, path.join(dataDir, 'serve.log')]] : ['ssh', ['-o', 'BatchMode=yes', HOST, `tail -c +${from + 1} '${dataDir}/serve.log'`]]))).out;
  const count = (text, re) => (text.match(re) || []).length;
  const statusAt = async () => { const r = await hostRun(['status', BIN, dataDir], { timeoutMs: 20_000 }); try { return JSON.parse(r.out); } catch { return { error: 'unparseable status', raw: r.out.slice(0, 2000) + r.err.slice(0, 500) }; } };
  let st1 = await statusAt();
  let join = ourJoin(st1);
  const tierOf = (status, j) => { const m0 = mainSession(status, j && j.joinId); return m0 ? m0.rate_tier || null : null; };
  R.quality = { requested: QUALITY == null && ADAPTIVE == null ? null : { quality: QUALITY == null ? null : +QUALITY, adaptive: ADAPTIVE }, join_id_before: join && join.joinId, rate_tier_before: tierOf(st1, join) };
  if (R.quality.requested) {
    if (!join || !join.token) throw new Error('host status has no join named ${DEVICE_NAME} with a device token; cannot pin quality');
    // The join seeds the device's override with the defaults (quality 15, adaptive off), and
    // `devices set` only renegotiates when the saved row actually changes, so pinning exactly the
    // seeded values would change nothing while the live session still runs its join-time
    // (adaptive) tier. Two steps therefore: a neighbouring quality first, which is a stream-class
    // change (epoch bump, renegotiation in place), then the requested one.
    const q = QUALITY == null ? 15 : +QUALITY;
    const nudge = q < 51 ? q + 1 : q - 1;
    const tSet = Date.now();
    R.quality.devices_set = [];
    let moved = false;
    for (const [i, qq] of [nudge, q].entries()) {
      const opts = ['--video-quality', String(qq), ...(ADAPTIVE ? ['--adaptive-quality', ADAPTIVE] : [])];
      const log0 = await serveLog();
      const reneg0 = count(log0, /is renegotiating in place/g), swap0 = count(log0, /closing the transport this renegotiation replaced/g);
      const ds = await hostRun(['devset', BIN, dataDir, join.token, ...opts], { timeoutMs: 30_000 });
      const rec = { args: opts, exit: Number(kv(ds.out).DEVSET_EXIT), renegotiated: false, output: ds.out.split('\n').filter((l) => l && !/^DEVSET_EXIT=/.test(l)).slice(0, 4) };
      // Done when the host's log shows the join renegotiating in place AND the transport it
      // replaced being closed (the new one is carrying the stream). 15 s for each step.
      for (let k = 0; k < 30 && !rec.renegotiated; k++) {
        await sleep(500);
        const l = await serveLog();
        if (count(l, /is renegotiating in place/g) > reneg0 && count(l, /closing the transport this renegotiation replaced/g) > swap0) rec.renegotiated = true;
      }
      st1 = await statusAt(); join = ourJoin(st1) || join;
      rec.rate_tier = tierOf(st1, join);
      R.quality.devices_set.push(rec);
      step(i ? 'quality-pinned' : 'quality-nudged', { args: opts.join(' '), exit: rec.exit, renegotiated: rec.renegotiated, tier: rec.rate_tier });
      if (!rec.renegotiated) R.failures.push(`quality step ${opts.join(' ')}: no in-place renegotiation seen in the host log within 15 s`);
    }
    moved = tierOf(st1, join) !== R.quality.rate_tier_before;
    let repaint = false;
    for (let i = 0; i < 20 && !repaint; i++) {
      const c0 = (await ev('SEVideo.getStats()').catch(() => ({ framesPainted: 0 }))).framesPainted; await sleep(500);
      const c1 = (await ev('SEVideo.getStats()').catch(() => ({ framesPainted: 0 }))).framesPainted;
      repaint = c1 > c0;
    }
    await sleep(1000);
    st1 = await statusAt(); join = ourJoin(st1) || join;
    Object.assign(R.quality, { rate_tier_after: tierOf(st1, join), tier_changed: moved, join_id_after: join && join.joinId, join_id_changed: !!(join && join.joinId !== R.quality.join_id_before), painting_after: repaint, settle_ms: Date.now() - tSet });
    step('quality-settled', { tier_after: R.quality.rate_tier_after, tier_changed: moved, join_id_changed: R.quality.join_id_changed, painting: repaint, ms: R.quality.settle_ms });
    if (!repaint) R.failures.push('page did not resume painting after the quality pin');
    if (ADAPTIVE === 'off' && /^Adaptive/.test(R.quality.rate_tier_after || '')) R.failures.push(`rate_tier still ${R.quality.rate_tier_after} after --adaptive off`);
  }
  const joinId = join && join.joinId;
  const hostStart = st1;
  R.client.paint_hook = [...workerSessions.values()];
  if (WINDOW_STATE && flag('headed')) {
    const w = await browser.send('Browser.getWindowForTarget', { targetId: tgt.id }).catch((e) => ({ error: e.message }));
    R.client.window_state = { requested: WINDOW_STATE, set: w.windowId ? await browser.send('Browser.setWindowBounds', { windowId: w.windowId, bounds: { windowState: WINDOW_STATE } }).then(() => 'ok', (e) => 'error ' + e.message) : w.error };
    await sleep(1000);
    R.client.window_state.visibility = await ev('document.visibilityState').catch(() => null);
  }
  if (TICK) await ev('window.__seTick && (window.__seTick.vs = [], window.__seTick.rvfc = [], window.__seTick.mdraw = [], window.__seTick.on = true)');
  if (DC_PING) await ev(`window.__seDc && window.__seDc.run(${DC_PING})`);
  const armed = (await inWorkers(PAINT_ARM)).some((x) => x.value === true);
  const a = await ev('SEVideo.getStats()'); const ta = Date.now(); const pa = await pcStats();

  const clockNow = () => ev('window.SEClock ? { offset_ms: SEClock.offsetMs, rtt_ms: SEClock.rttMs, samples: SEClock.samples } : null').catch(() => null);
  R.client.clock = { start: await clockNow() };
  const cpu0 = kv((await hostRun(['cputime', String(R.host.pid)])).out).CPU;
  const powerP = POWER ? hostRun(['powersample', String(R.host.pid), String(DURATION)], { timeoutMs: (DURATION + 60) * 1000 }) : null;
  let winP = null;
  if (MOTION_ARG && MOTION_ARG.kind === 'window') {
    const how = MOTION_LAUNCH === 'auto' ? (LOCAL ? 'direct' : 'open') : MOTION_LAUNCH;
    R.motion = { requested: true, kind: 'window', mode: MOTION_ARG.mode, launch: how, display: vd && vd.id, ran: false };
    if (R.screen_locked) { R.motion.reason = 'host screen locked'; R.failures.push('--motion window on a locked host screen'); }
    else if (!vd) { R.motion.reason = 'virtual display not identified'; R.failures.push('--motion window: no virtual display to draw on'); }
    else {
      const out = `/tmp/se-motion-${crypto.randomBytes(4).toString('hex')}.json`;
      winP = hostRun(['motionwin', motionWin, String(vd.id), MOTION_ARG.mode, String(DURATION + 1), out, how], { timeoutMs: (DURATION + 60) * 1000 });
      R.motion.ran = true;
    }
  }
  let moverProc = null; let moverOut = '';
  if (MOTION && MOTION_ARG.kind === 'cursor') {
    if (R.screen_locked) R.motion = { requested: true, ran: false, reason: 'host screen locked (CGSSessionScreenIsLocked=Yes); the capture shows the lock screen, so motion would not reach the stream' };
    else if (!vd) R.motion = { requested: true, ran: false, reason: 'could not identify the virtual display (no new display id after the join)' };
    else {
      const args = ['move', mover, String(vd.id), String(DURATION)];
      moverProc = LOCAL ? spawn('/bin/bash', [LAUNCHER, ...args]) : spawn('ssh', ['-o', 'BatchMode=yes', HOST, 'bash', '-s', '--', ...args]);
      if (!LOCAL) moverProc.stdin.end(fs.readFileSync(LAUNCHER, 'utf8')); else moverProc.stdin.end();
      moverProc.stdout.on('data', (d) => { moverOut += d; }); moverProc.stderr.on('data', (d) => { moverOut += d; });
      R.motion = { requested: true, ran: true, display: vd.id };
    }
  }
  step('measuring', { seconds: DURATION, motion: R.motion.ran, screen_locked: R.screen_locked, virtual_display: vd && `${vd.id} ${vd.w}x${vd.h}` });
  const samples = [];
  const logStart = Buffer.byteLength(await serveLog());
  let logOff = logStart, logWindow = '';
  const bwe = [];
  let prevPc = pa;
  const d = (x, y) => (x != null && y != null ? x - y : null);
  for (let i = 0; i < DURATION; i++) {
    await sleep(Math.max(0, ta + (i + 1) * 1000 - Date.now()));
    const tS = (Date.now() - ta) / 1000;
    // The worker's live counters, straight from the worker: the page's own stats are a once-a-second
    // copy that silently goes stale when the worker hangs. `null` = no reply in 600 ms (wedged).
    const wk = await Promise.race([inWorkers(`(function () { var o = {}; ['framesIn', 'framesDecoded', 'framesRendered', 'framesDroppedBacklog', 'decodeErrors', 'keyRequests', 'inFlight', 'waitingForKey', 'submitsSinceOutput', 'silentTrips', 'configured', 'decoderResets'].forEach(function (k) { try { o[k] = eval(k); } catch (_) {} }); try { o.queue = decoder ? decoder.decodeQueueSize : -1; o.state = decoder ? decoder.state : 'none'; o.sinceOutputMs = lastOutputAtMs ? Math.round(performance.now() - lastOutputAtMs) : null; } catch (_) {} return o; })()`).then((v) => (v.map((x) => x.value).filter((x) => x && typeof x.framesIn === 'number').sort((x, y) => y.framesIn - x.framesIn)[0] || 'no-video-worker')), sleep(600).then(() => null)]);
    const [s, pc, chunk, hs] = await Promise.all([ev('SEVideo.getStats()').catch(() => null), pcStats(), serveLog(logOff), HOST_SERIES ? statusAt() : null]);
    if (hs) {
      const ms = mainSession(hs, joinId);
      if (ms) (R.host.series = R.host.series || []).push({ t: +tS.toFixed(2), ...Object.fromEntries(Object.entries(ms).filter(([, v]) => v === null || typeof v !== 'object')), degradation: Object.fromEntries(Object.entries(ms.degradation || {}).filter(([k]) => /hold|suspen|repeat|idr|qp|rate|drop/i.test(k))) });
    }
    logOff += Buffer.byteLength(chunk); logWindow += chunk;
    for (const line of chunk.split('\n')) {
      const m = /^BWE: pushing adaptive bitrate target \(measured_send_bps=(\d+), delivered_bps=(\d+), loss=([\d.]+), rtt_ms=([\d.]+)(?:, delay=([^,]+))?(?:, estimate=(\d+))?(?:, target=(\d+))?/.exec(line);
      if (m) bwe.push({ t: +tS.toFixed(2), measured_send_bps: +m[1], delivered_bps: +m[2], loss: +m[3], rtt_ms: +m[4], delay: m[5] || null, estimate: m[6] ? +m[6] : null, target: m[7] ? +m[7] : null });
      else if (/^BWE:/.test(line)) bwe.push({ t: +tS.toFixed(2), line: line.slice(0, 240) });
    }
    const rtc = pc && prevPc ? { packets: d(pc.packets, prevPc.packets), lost: d(pc.lost, prevPc.lost), nack: d(pc.nack, prevPc.nack), pli: d(pc.pli, prevPc.pli), mbps: pc.bytes != null && prevPc.bytes != null ? +(((pc.bytes - prevPc.bytes) * 8) / 1e6).toFixed(2) : null, ice_rtt_ms: pc.ice_rtt_ms != null ? +pc.ice_rtt_ms.toFixed(1) : null } : null;
    if (pc) prevPc = pc;
    if (s) samples.push({ t: +tS.toFixed(2), painted: s.framesPainted, dropped: s.framesDropped, decode_errors: s.decodeErrors, key_requests: s.keyRequests, rtt_ms: s.rttMs, a2p: s.arrivalToPaint, decode: s.decode, h2p: s.hostToPaint, rtc, worker: wk });
  }
  R.host.bwe = bwe;
  const b = await ev('SEVideo.getStats()'); const tb = Date.now(); const pb = await pcStats();
  R.client.paint_timing = armed
    ? ((await inWorkers(PAINT_READ)).map((x) => x.value).filter((v) => v && v.paints != null).sort((x, y) => y.paints - x.paints)[0] || { error: 'no worker painted' })
    : { error: `paint hook not armed (workers: ${JSON.stringify(R.client.paint_hook)})` };
  if (TICK) R.tick = await collectTick(ev, inWorkers);
  if (DC_PING) {
    for (let i = 0; i < 20 && !(await ev('!!(window.__seDc && window.__seDc.done)').catch(() => true)); i++) await sleep(500);
    await sleep(500);
    const d = await ev('window.__seDc ? { rtt: window.__seDc.rtt, pong_abs: window.__seDc.pong_abs, unanswered: window.__seDc.sent.size, others: window.__seDc.others, channel: !!window.__seDc.ch } : null').catch((e) => ({ error: e.message }));
    const pq = (a) => { const s2 = a.slice().sort((x, y) => x - y); const q = (p) => (s2.length ? +s2[Math.min(s2.length - 1, Math.floor(p * s2.length))].toFixed(3) : null); return { n: s2.length, min: q(0), p10: q(0.1), p50: q(0.5), p90: q(0.9), p99: q(0.99), max: s2.length ? +s2[s2.length - 1].toFixed(3) : null }; };
    const lock = (ts, P) => { if (!ts.length) return null; let c = 0, sn = 0; for (const t of ts) { const a = (2 * Math.PI * (t % P)) / P; c += Math.cos(a); sn += Math.sin(a); } return +(Math.hypot(c, sn) / ts.length).toFixed(3); };
    R.dc_ping = d && d.rtt ? { requested: DC_PING, answered: d.rtt.length, unanswered: d.unanswered, other_pongs: d.others, rtt_ms: pq(d.rtt), pong_phase_lock: { '15.625ms': lock(d.pong_abs, 1000 / 64), '16.667ms': lock(d.pong_abs, 1000 / 60), '10ms': lock(d.pong_abs, 10) }, rtt_raw: d.rtt.map((x) => +x.toFixed(3)) } : d;
  }
  R.client.frame_meta = (await inWorkers(META_READ)).map((x) => x.value).filter((v) => v && v.a2p).sort((x, y) => y.a2p.n - x.a2p.n)[0] || null;
  R.client.clock.end = await clockNow();
  const cpu1 = kv((await hostRun(['cputime', String(R.host.pid)])).out).CPU;
  const cpuS = (t) => { if (!t) return null; const p = t.split(':').map(Number); return p.reduce((a, x) => a * 60 + x, 0); };
  R.host.cpu = { start: cpu0, end: cpu1, seconds: cpuS(cpu1) != null && cpuS(cpu0) != null ? +(cpuS(cpu1) - cpuS(cpu0)).toFixed(2) : null };
  if (R.host.cpu.seconds != null) R.host.cpu.pct_of_one_core = +((100 * R.host.cpu.seconds) / ((Date.now() - ta) / 1000)).toFixed(1);
  if (CPU_THROTTLE) {
    // Is the worker really throttled? Time the same busy loop throttled and unthrottled.
    const busy = '(() => { const t0 = performance.now(); let x = 0; for (let i = 0; i < 3e7; i++) x += i % 7; return performance.now() - t0 + (x < 0 ? 1 : 0); })()';
    const vw = [...workerSessions.entries()].find(([, w]) => w.hook === 'hooked' && /blob:|transform/.test(w.url));
    if (vw) {
      const t1 = await page.send('Runtime.evaluate', { expression: busy, returnByValue: true }, vw[0]).then((r) => r.result.value, () => null);
      await page.send('Emulation.setCPUThrottlingRate', { rate: 1 }, vw[0]).catch(() => {}); await page.send('Emulation.setCPUThrottlingRate', { rate: 1 }).catch(() => {});
      const t2 = await page.send('Runtime.evaluate', { expression: busy, returnByValue: true }, vw[0]).then((r) => r.result.value, () => null);
      R.client.cpu_throttle.worker_busy_loop_ms = { throttled: t1 && +t1.toFixed(1), unthrottled: t2 && +t2.toFixed(1), ratio: t1 && t2 ? +(t1 / t2).toFixed(2) : null };
    }
  }
  // The fixed worker's own counter (a top-level binding in the worker; absent in the old one).
  // The fixed worker's own guard counters (top-level bindings in the worker; absent in the
  // shipped one, so null there).
  const guards = (await inWorkers(`(function () { var o = {}; ['keyDecodeGraceHits', 'stallFloorSaves', 'stallFloorSavesKey', 'stallFloorSavesGap', 'stallFloorSavesIdle', 'framesDroppedBacklog', 'keyRequests', 'silentTrips', 'seDeadAt', 'seKeyReqAt', 'seRevivedAt', 'seSwallowed'].forEach(function (k) { try { o[k] = eval(k); } catch (_) { o[k] = null; } }); return o; })()`)).map((x) => x.value).filter((v) => v && typeof v.keyRequests === 'number');
  R.client.worker_guards = guards.sort((x, y) => (y.keyRequests || 0) - (x.keyRequests || 0))[0] || null;
  R.client.key_decode_grace_hits = R.client.worker_guards ? R.client.worker_guards.keyDecodeGraceHits : null;
  const hostEnd = await statusAt();
  if (moverProc) { await new Promise((r) => { moverProc.on('close', r); setTimeout(r, 5000); }); try { R.motion.result = JSON.parse(moverOut.trim().split('\n').pop()); } catch { R.motion.result = moverOut.trim().slice(0, 500); } }
  if (winP) {
    const r = await winP;
    const raw = kv(r.out).MOTION || '';
    try { R.motion.result = JSON.parse(raw); } catch { R.motion.result = { error: 'no result from motion-window', raw: (raw || r.out + r.err).slice(0, 400) }; }
    R.motion.presented_fps = R.motion.result && R.motion.result.presented_fps != null ? R.motion.result.presented_fps : null;
  }
  if (powerP) {
    const r = await powerP;
    const rows = r.out.split('\n').filter((l) => l.startsWith('PS ')).map((l) => Object.fromEntries(l.slice(3).trim().split(/\s+/).map((x) => x.split('='))));
    const stat = (k) => { const a = rows.map((x) => Number(x[k])).filter((x) => isFinite(x)).sort((x, y) => x - y); if (!a.length) return null; const q = (p) => a[Math.min(a.length - 1, Math.floor(p * a.length))]; return { n: a.length, mean: Math.round(a.reduce((x, y) => x + y, 0) / a.length), p50: q(0.5), p90: q(0.9), min: a[0], max: a[a.length - 1] }; };
    // PowerTelemetryData refreshes about once a minute (verified on the M1: the sample count
    // moves by ~59 per refresh). Between two refreshes seen inside the window, delta Accumulated /
    // delta Count is the exact mean over that span; that is the number to compare, and the
    // window has to span two refreshes (>= ~120 s) to contain one whole interval.
    const ups = [];
    for (const x of rows) { const c = Number(x.SystemLoadAccumulatorCount); if (isFinite(c) && (!ups.length || ups[ups.length - 1].count !== c)) ups.push({ t: Number(x.t), count: c, acc_load: Number(x.AccumulatedSystemLoad), acc_in: Number(x.AccumulatedSystemPowerIn), in_count: Number(x.SystemPowerInAccumulatorCount) }); }
    const inside = ups.slice(1); // the first row is whatever value was current when the window began
    const span = inside.length >= 2 ? { from_t: inside[0].t, to_t: inside[inside.length - 1].t, seconds_covered: inside[inside.length - 1].count - inside[0].count,
      mean_system_load_mw: Math.round((inside[inside.length - 1].acc_load - inside[0].acc_load) / (inside[inside.length - 1].count - inside[0].count)),
      mean_system_power_in_mw: Math.round((inside[inside.length - 1].acc_in - inside[0].acc_in) / (inside[inside.length - 1].in_count - inside[0].in_count)) } : null;
    R.power = { source: rows.some((x) => x.SystemPowerIn != null) ? 'ioreg AppleSmartBattery PowerTelemetryData (mW; refreshed ~1/min)' : 'none on this host', refreshes_in_window: inside.length, accumulator_mean: span,
      system_power_in_mw: stat('SystemPowerIn'), system_load_mw: stat('SystemLoad'), samples: rows };
  }

  // ---- client summary
  const VP = { 0: 'none', 1: 'webcodecs-canvas', 2: 'webcodecs-video(track generator)', 3: 'video-element' };
  const TT = { 0: 'none', 1: 'RTCRtpScriptTransform', 2: 'createEncodedStreams' };
  const RT = { 0: 'none', 1: 'canvas2d', 2: 'VideoTrackGenerator', 3: '<video>' };
  const secs = (tb - ta) / 1000;
  R.client.video = {
    path_code: b.videoPath, path: VP[b.videoPath], transport_tier: TT[b.transportTier], render_tier: RT[b.renderTier], key_tier_code: b.keyTier,
    decoder_hardware: b.decoderHardware, clock_valid: b.clockValid, cross_origin_isolated: b.crossOriginIsolated,
    window_s: +secs.toFixed(2),
    frames_painted_total: b.framesPainted, frames_painted_window: b.framesPainted - a.framesPainted, painted_fps: +((b.framesPainted - a.framesPainted) / secs).toFixed(2),
    frames_dropped_total: b.framesDropped, frames_dropped_window: b.framesDropped - a.framesDropped,
    decode_errors: b.decodeErrors, keyframe_requests_total: b.keyRequests, keyframe_requests_window: b.keyRequests - a.keyRequests,
    arrival_to_paint_ms: b.arrivalToPaint, decode_ms: b.decode, host_capture_to_paint_ms: b.hostToPaint, rtt_ms: b.rttMs,
    degraded_flags: b.degraded, suppressed_errors: b.suppressed,
  };
  R.client.webrtc = { start: pa, end: pb };
  if (pa && pb) {
    const recv = d(pb.packets, pa.packets), lost = d(pb.lost, pa.lost);
    R.client.webrtc_window = { packets_received: recv, packets_lost: lost, lost_pct: recv != null && lost != null && recv + lost > 0 ? +((100 * lost) / (recv + lost)).toFixed(2) : null, nack: d(pb.nack, pa.nack), pli: d(pb.pli, pa.pli), mbps: pb.bytes != null ? +((((pb.bytes - pa.bytes) * 8) / 1e6) / secs).toFixed(2) : null };
  }
  {
    const fullLog = await serveLog();
    const reasons = (text) => { const o = {}; for (const line of text.split('\n')) { const m = /^(keyframe requested|picture recovery): (.*)$/.exec(line); if (m) { const k = (m[1] === 'picture recovery' ? 'repair: ' : '') + m[2].replace(/\d+(\.\d+)?/g, '#').slice(0, 120); o[k] = (o[k] || 0) + 1; } } return o; };
    R.host.keyframe_requests_by_reason = { session: reasons(fullLog), window: reasons(logWindow) };
    const bw = bwe.filter((x) => x.loss != null);
    R.host.bwe_summary = bw.length ? { pushes: bw.length, worst_loss: Math.max(...bw.map((x) => x.loss)), worst_rtt_ms: Math.max(...bw.map((x) => x.rtt_ms)), max_measured_send_mbps: +(Math.max(...bw.map((x) => x.measured_send_bps)) / 1e6).toFixed(2) } : { pushes: 0 };
  }
  R.client.samples = samples;
  R.client.raw_end = b;
  R.client.console = page.events.filter((e) => e.method === 'Log.entryAdded' || e.method === 'Runtime.exceptionThrown' || (e.method === 'Runtime.consoleAPICalled' && /error|warn/.test(e.params.type)))
    .map((e) => (e.method === 'Log.entryAdded' ? `${e.params.entry.level}: ${e.params.entry.text}` : e.method === 'Runtime.exceptionThrown' ? 'EXC ' + JSON.stringify(e.params.exceptionDetails).slice(0, 300) : `${e.params.type}: ${e.params.args.map((x) => x.value || x.description || '').join(' ')}`).slice(0, 300)).slice(0, 60);

  // ---- host summary
  R.host.status_start = hostStart; R.host.status_end = hostEnd;
  const m = mainSession(hostEnd, joinId);
  const jEnd = ourJoin(hostEnd) || join;
  const disp = jEnd && jEnd.display;
  const capLine = (await (async () => { const t = await serveLog(); return t.split('\n').filter((l) => /\] capture started: display=/.test(l)).pop() || null; })());
  const originLines = (await serveLog()).split('\n').filter((l) => /remote-input (display|geometry)|came up at|set to \d+x\d+|re-applied/.test(l)).slice(0, 12);
  R.display = { origin_log: originLines, cg_bounds: vd ? { x: vd.x, y: vd.y, w: vd.w, h: vd.h } : null, overlaps_another_display: (R.host.display_overlaps || []).some((pr) => vd && pr.includes(vd.id)),
    requested: SIZE, actual: disp ? `${disp.width}x${disp.height}` : null, refresh_hz: disp ? disp.refresh : null, scale: disp ? disp.scale : null, capture_log: capLine,
    page_screen: await ev('screen.width + "x" + screen.height + "@" + devicePixelRatio').catch(() => null), decoded_frame: pb ? `${pb.width}x${pb.height}` : null };
  if (R.display.actual !== SIZE) R.failures.push(`display ${R.display.actual} != requested ${SIZE}`);
  {
    // r6+: "[clock] host_now_ns=<n> unix_ns=<n>" pairs the host clock with wall time. Truth for the
    // page's offset (hostMs - absMs) is host_now_ms - unix_ms; on the same machine the page's
    // absolute clock is the same wall clock, so the difference is the estimator's error.
    const full = await serveLog();
    const cl = [...full.matchAll(/\[clock\][^\n]*?host_now_ns=(\d+)[^\n]*?unix_ns=(\d+)/g)].map((m) => ({ host_now_ns: m[1], unix_ns: m[2], true_offset_ms: Number((BigInt(m[1]) - BigInt(m[2])) / 1000n) / 1000 }));
    const page = R.client.clock && R.client.clock.end;
    const sameMachine = !CLIENT && LOCAL;
    R.clock_check = { lines: cl.slice(0, 4), page_offset_ms: page ? page.offset_ms : null, page_rtt_ms: page ? page.rtt_ms : null, same_machine: sameMachine || (CLIENT && CLIENT === HOST),
      error_ms: cl.length && page && page.offset_ms != null ? +(page.offset_ms - cl[cl.length - 1].true_offset_ms).toFixed(3) : null };
  }
  if (m) {
    const deg = m.degradation || {};
    R.host.session = {
      key: m.key, capture_backend: m.capture_backend, capture_features: m.capture_features, encoder: m.encoder, encoder_id: m.encoder_id ?? null,
      rate_tier: m.rate_tier ?? null, hold_push_mode: m.hold_push_mode ?? null, host_power: m.host_power ?? null,
      frames_encoded: m.frames_encoded, frames_submitted: m.frames_submitted ?? null, bytes_sent: m.bytes_sent,
      frames_encoded_window: (() => { const s0 = mainSession(hostStart, joinId); return s0 ? m.frames_encoded - s0.frames_encoded : null; })(),
      wire: (() => { const s0 = mainSession(hostStart, joinId); if (!s0) return null; const dt = R.client.video.window_s; return { mbps: +((((m.bytes_sent - s0.bytes_sent) * 8) / 1e6) / dt).toFixed(2), frames_per_s: +((m.frames_encoded - s0.frames_encoded) / dt).toFixed(2), submitted_per_s: m.frames_submitted != null && s0.frames_submitted != null ? +((m.frames_submitted - s0.frames_submitted) / dt).toFixed(2) : null }; })(),
      stages: m.stages, degradation: deg, client_reported: m.client,
      // Whatever else the session object carries that bears on repeats, holds and tuning
      // (encode_repeat lives in stages; rtp_ts_clamped, hold_push_bytes, perf-hold/tuning fields
      // appear only when non-empty).
      extras: Object.fromEntries(Object.entries(m).filter(([k]) => /repeat|clamp|hold|perf|tuning|power|synthetic|source|suspen|qp|ceiling|cap|reason|clock/i.test(k))),
      // Everything the session object carries beyond the fields this harness was written against,
      // so a new build's telemetry is recorded even before anyone names it here.
      new_fields: Object.fromEntries(Object.entries(m).filter(([k]) => !['key', 'capture_backend', 'capture_features', 'encoder', 'frames_encoded', 'bytes_sent', 'stages', 'degradation', 'client', 'frames_submitted', 'host_power', 'encoder_id', 'rate_tier', 'hold_push_bytes', 'hold_push_mode', 'rtp_ts_clamped'].includes(k))),
      new_degradation_fields: Object.keys(deg).filter((k) => !['audio_broadcast_lag_drops', 'bitrate_cuts', 'capture_fallback', 'capture_no_first_frame', 'client_jitter_overruns', 'client_jitter_underruns', 'client_keyframe_requests', 'cursor_composite_us', 'cursor_composites', 'cursor_frames', 'cursor_shape_errors', 'delay_overuse_events', 'encoder_output_drops', 'encoder_rate_drops', 'encoder_resets', 'encoder_tier_fallbacks', 'encoder_transient_errors', 'frames_dropped_broadcast', 'frames_dropped_capture', 'frames_dropped_ring', 'frames_dropped_stale', 'frames_hold_pushed', 'frames_repeated', 'frames_skipped_cadence', 'frames_skipped_unchanged', 'idrs_sent', 'input_parse_errors', 'input_queue_drops', 'input_reorder_drops', 'input_stale_drops', 'keyframe_requests', 'keyframe_requests_suppressed', 'nacks_received', 'pacer_deadline_misses'].includes(k)),
    };
    R.host.counters = Object.fromEntries(Object.entries(deg).filter(([k]) => /^frames_dropped|pacer_deadline_misses|keyframe_requests|client_keyframe_requests|nacks_received|encoder_rate_drops|frames_repeated|frames_hold_pushed|idrs_sent|encoder_output_drops|encoder_tier_fallbacks/.test(k)));
    if (SYNTHETIC && !String(m.capture_backend || '').startsWith('synthetic:')) R.failures.push(`--synthetic ${SYNTHETIC} requested but the host captured with ${m.capture_backend}`);
    // Fresh captures in the window: capture->encode only counts frames a capture delivered
    // (repeats and hold pushes do not pass through it).
    const s0 = mainSession(hostStart, joinId);
    // A stage that has not fired yet is absent from the JSON: that is zero captures, not unknown.
    const cnt = (x, k) => (x && x.stages ? (x.stages[k] ? x.stages[k].count : 0) : null);
    const fresh = s0 && cnt(m, 'capture_to_encode') != null && cnt(s0, 'capture_to_encode') != null ? cnt(m, 'capture_to_encode') - cnt(s0, 'capture_to_encode') : null;
    R.host.session.fresh_captures_window = fresh;
    R.host.session.fresh_captures_per_s = fresh != null ? +(fresh / R.client.video.window_s).toFixed(2) : null;
    if (MOTION_ARG && MOTION_ARG.kind === 'window') {
      if (String(m.capture_backend || '').startsWith('synthetic')) R.failures.push('--motion window but the host capture backend is synthetic');
      const pf = R.motion.presented_fps;
      if (!(pf > 0)) R.failures.push('--motion window: the helper presented nothing (' + String(JSON.stringify(R.motion.result)).slice(0, 160) + ')');
      else if (!(R.host.session.fresh_captures_per_s >= 0.8 * pf)) R.failures.push(`host fresh captures ${R.host.session.fresh_captures_per_s}/s < 80 % of the ${pf} fps the helper presented`);
    }
  } else R.failures.push('host status --json reported no session');

  // ---- verdict
  if (RELAY_STALL && !(R.client.relay && R.client.relay.used)) R.failures.push('--relay-stall requested but the relay was not used: no stall was induced');
  if (!(b.framesPainted > 0)) R.failures.push('no frame painted');
  if (b.decodeErrors > 0) R.failures.push(`decode errors: ${b.decodeErrors}`);
  const rateDrops = sessionsOf(hostEnd).reduce((n, x) => n + ((x.degradation || {}).encoder_rate_drops || 0), 0);
  if (rateDrops > 0) R.failures.push(`encoder_rate_drops: ${rateDrops}`);
  if (!(b.framesPainted > a.framesPainted)) R.failures.push('no frame painted during the measured window');
  for (const o of R.client.overrides || []) if (!o.served) R.failures.push(`--client-override ${o.name} was never requested by the page`);
  R.motion_active = !!(SYNTHETIC || R.motion.ran);
  if (R.motion_active) {
    const fps = R.client.video.painted_fps;
    if (!(fps >= 0.9 * EXPECT_FPS)) R.failures.push(`painted ${fps} fps < 90 % of ${EXPECT_FPS}`);
    if (R.client.video.frames_dropped_window > 0) R.failures.push(`client dropped ${R.client.video.frames_dropped_window} frames`);
  }

  // ---- leave cleanly, close the browser
  await page.send('Page.navigate', { url: 'about:blank' }).catch(() => {}); await sleep(1200);
  await browser.send('Browser.close').catch(() => {});
  if (CLIENT) { await sleep(1500); chromeExited = !sshC(`pgrep -f '${remoteChrome.profile}'`).out.trim(); }
  for (let i = 0; i < 50 && !chromeExited; i++) await sleep(100);
  step('chrome-closed', { exited: chromeExited });
}

function table() {
  const v = R.client.video || {}, h = R.host.session || {}, d = h.degradation || {};
  const p = (o) => (o ? `${fmt(o.p50)}/${fmt(o.p90)}/${fmt(o.p99)}` : '-');
  const fmt = (x) => (x == null ? '-' : typeof x === 'number' ? (Math.abs(x) >= 100 ? x.toFixed(0) : x.toFixed(2)) : x);
  const lines = [
    `== ${R.label}  ${R.pass ? 'PASS' : 'FAIL'}  ${R.failures.join('; ')}`,
    `host ${R.host.ip} ${R.host.os || ''}   screen_locked=${R.screen_locked}   motion=${R.motion.ran ? 'ran' : 'no'}${R.motion.reason ? ' (' + R.motion.reason.split(';')[0] + ')' : ''}`,
  ];
  if (!R.client.video) return lines.concat([`no measurement (failed before the measured window)`, `json: ${OUT}`]).join('\n');
  lines.push(
    `client→host: ${R.client.relay && R.client.relay.used ? `VIA LOOPBACK RELAY (${R.client.relay.mode === 'on' ? 'forced' : 'Chrome lacks macOS Local Network access'})` : 'direct'}`,
    `join: submit→first paint ${fmt(R.join && R.join.ms_submit_to_first_paint)} ms   host startup ${fmt(R.host.startup_ms)} ms`,
    `display: requested ${R.display && R.display.requested}, actual ${R.display && R.display.actual} @ ${R.display && R.display.refresh_hz} Hz (decoded ${R.display && R.display.decoded_frame})   synthetic: ${SYNTHETIC || 'no'} (host env: ${R.host.process_env_se || '-'})`,
    `client overrides: ${(R.client.overrides || []).map((o) => `${o.name} sha256 ${o.sha256.slice(0, 12)} served ${o.served}x`).join(', ') || 'none'}`,
    `quality: ${R.quality && R.quality.requested ? `q=${QUALITY} adaptive=${ADAPTIVE}  tier ${R.quality.rate_tier_before} -> ${R.quality.rate_tier_after}${R.quality.join_id_changed ? ' (join id changed)' : ''}` : `not pinned (tier ${R.quality && R.quality.rate_tier_before})`}`,
    `paint timing: ${R.client.paint_timing && R.client.paint_timing.paints != null ? `${R.client.paint_timing.fps} fps, interval p50/p90/p99 ${R.client.paint_timing.interval_p50_ms}/${R.client.paint_timing.interval_p90_ms}/${R.client.paint_timing.interval_p99_ms} ms, longest gap ${R.client.paint_timing.longest_gap_ms} ms, gaps>100ms ${R.client.paint_timing.gaps_over_100ms}` : JSON.stringify(R.client.paint_timing)}`,
    `motion: ${JSON.stringify(R.motion).slice(0, 220)}   host fresh captures/s ${h.fresh_captures_per_s}`,
    `frame split ms p50/p90/p99 (n): ${R.client.frame_meta ? ['c2r', 'r2a', 'a2p', 'c2p'].map((k) => `${k} ${p(R.client.frame_meta[k])} (${R.client.frame_meta[k] && R.client.frame_meta[k].n})`).join('  ') + '  fields ' + JSON.stringify(R.client.frame_meta.fields) : '-'}`,
    `clock: ${JSON.stringify(R.client.clock)}   host cpu ${JSON.stringify(R.host.cpu)}${R.power ? '   power SystemPowerIn mW ' + JSON.stringify(R.power.system_power_in_mw) : ''}`,
    `video path: ${v.path} / transport ${v.transport_tier} / render ${v.render_tier} / hw decode ${v.decoder_hardware}`,
    `painted ${v.frames_painted_window} in ${v.window_s}s (${v.painted_fps} fps), dropped ${v.frames_dropped_window}, decode errors ${v.decode_errors}, key requests ${v.keyframe_requests_window}`,
    `client ms p50/p90/p99: arrival→paint ${p(v.arrival_to_paint_ms)}  decode ${p(v.decode_ms)}  host-capture→paint ${p(v.host_capture_to_paint_ms)}  rtt ${fmt(v.rtt_ms)}`,
    `host: capture ${h.capture_backend}  encoder ${h.encoder}  id=${h.encoder_id}  rate_tier=${h.rate_tier}  hold_push=${h.hold_push_mode}  encoded(window) ${h.frames_encoded_window}`,
  );
  for (const [k, s] of Object.entries(h.stages || {})) if (s && s.count) lines.push(`  stage ${k.padEnd(18)} n=${String(s.count).padStart(6)}  p50 ${fmt(s.p50_ms ?? s.p50)}  p90 ${fmt(s.p90_ms ?? s.p90)}  p99 ${fmt(s.p99_ms ?? s.p99)}  max ${fmt(s.max_ms ?? s.max)}`);
  const nz = Object.entries(d).filter(([, x]) => x);
  lines.push(`degradation (non-zero): ${nz.map(([k, x]) => `${k}=${x}`).join(' ') || 'none'}`);
  if (h.extras && Object.keys(h.extras).length) lines.push(`host extras: ${JSON.stringify(h.extras)}`);
  lines.push(`virtual display removed: ${R.host.virtual_display_removed}   left behind: ${JSON.stringify({ pid: R.host.stop && R.host.stop.left_pid, listeners: R.host.stop && R.host.stop.left_listeners })}`);
  lines.push(`json: ${OUT}`);
  return lines.join('\n');
}

let finishing = false;
async function finish(code, why) {
  if (finishing) return; finishing = true;
  try { await cleanup(why); } catch (e) { R.failures.push('cleanup: ' + e.message); }
  R.finished_at = now(); R.elapsed_ms = Date.now() - T_START;
  R.pass = R.failures.length === 0 && code === 0;
  if (!R.pass && code === 0) code = 2;
  fs.writeFileSync(OUT, JSON.stringify(R, null, 2));
  console.log('\n' + table());
  process.exit(code);
}
process.on('SIGINT', () => { R.failures.push('interrupted'); finish(1, 'SIGINT'); });
process.on('SIGTERM', () => { R.failures.push('terminated'); finish(1, 'SIGTERM'); });
main().then(() => finish(0, 'done'), (e) => { R.error = e.message; if (!R.failures.includes(e.message)) R.failures.push(e.message); finish(/permission|busy/i.test(e.message) ? 3 : 1, 'error'); });
