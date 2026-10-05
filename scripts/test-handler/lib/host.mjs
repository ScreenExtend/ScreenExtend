// The host under test: what a build offers, starting `serve`, talking to it, stopping it.
import { spawn, execFile } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pidAlive } from './slots.mjs';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const ALPHABET = '23456789ABCDEFGHJKLMNPQRSTUVWXYZ';
export const randomSessionId = () => Array.from(crypto.randomBytes(12), (b) => ALPHABET[b % ALPHABET.length]).join('');
export const randomOtp = () => String(crypto.randomInt(0, 1000000)).padStart(6, '0');

/** What the binary contains, found by scanning it for string literals. Nothing is executed. */
export function probeBuild(exe) {
  const buf = fs.readFileSync(exe);
  const has = (s) => buf.indexOf(Buffer.from(s, 'latin1')) >= 0;
  const dir = path.dirname(exe);
  const st = fs.statSync(exe);
  return {
    exe, size: st.size, mtime: st.mtime.toISOString(),
    sha256: crypto.createHash('sha256').update(buf).digest('hex'),
    isolationSwitch: has('SCREENEXTEND_DATA_DIR'),
    envCredentials: has('SCREENEXTEND_OTP') && has('SCREENEXTEND_SESSION_ID'),
    controlStats: has('STATS') && has('telemetry'),
    // The client page is compiled in; these tell which page generation it is.
    pageHasSEVideo: has('window.SEVideo'),
    pageHasJoinId: has('X-Join-Id'),
    dlls: { 'libopus.dll': fs.existsSync(path.join(dir, 'libopus.dll')), 'libx264-164.dll': fs.existsSync(path.join(dir, 'libx264-164.dll')) },
  };
}

export function runCli(exe, args, { env = {}, timeout = 20000 } = {}) {
  return new Promise((resolve) => {
    const t0 = Date.now();
    execFile(exe, args, { env: { ...process.env, ...env }, timeout, windowsHide: true, encoding: 'utf8', maxBuffer: 16 << 20 }, (err, stdout, stderr) => {
      resolve({ args, code: err ? (err.code ?? 1) : 0, ms: Date.now() - t0, stdout: stdout || '', stderr: stderr || '', error: err && err.killed ? 'timeout' : undefined });
    });
  });
}

export function cliJson(r) {
  if (r.code !== 0) return null;
  const s = r.stdout; const i = s.indexOf('{');
  if (i < 0) return null;
  try { return JSON.parse(s.slice(i)); } catch { return null; }
}

/**
 * Start `serve`. opts: { exe, dataDir (null = real data dir), httpPort, httpsPort, sessionId, otp,
 * envCredentials, extraArgs, extraEnv, logFile }.
 */
export async function startHost(o) {
  const args = ['serve', '--no-cloud', '--no-qr', '-v', '--http-port', String(o.httpPort), '--https-port', String(o.httpsPort)];
  const env = { ...process.env, ...o.extraEnv };
  delete env.SCREENEXTEND_DATA_DIR; delete env.SCREENEXTEND_SESSION_ID; delete env.SCREENEXTEND_OTP;
  if (o.dataDir) env.SCREENEXTEND_DATA_DIR = o.dataDir;
  let credentialsVia;
  if (o.envCredentials) { env.SCREENEXTEND_SESSION_ID = o.sessionId; env.SCREENEXTEND_OTP = o.otp; credentialsVia = 'environment'; }
  else { args.push('--session-id', o.sessionId, '--otp', o.otp); credentialsVia = 'argv (this build reads no credential environment variables)'; }
  args.push(...(o.extraArgs || []));
  // Every line is prefixed with the system clock as it reaches us ("[epoch-ms] "), so the host's
  // log can be laid against the browser's network log: `serve` prints no times of its own. The
  // prefix is when the handler read the line (a pipe, read on the event loop), which trails the
  // host's write by well under a millisecond on an idle loop and by a few ms on a busy one.
  const out = fs.openSync(o.logFile, 'a');
  const proc = spawn(o.exe, args, { env, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
  for (const s of [proc.stdout, proc.stderr]) {
    let tail = '';
    s.setEncoding('utf8');
    s.on('data', (d) => {
      const now = Date.now(); const parts = (tail + d).split('\n'); tail = parts.pop();
      if (parts.length) { try { fs.writeSync(out, parts.map((l) => `[${now}] ${l.replace(/\r$/, '')}\n`).join('')); } catch { /* closed */ } }
    });
    s.on('end', () => { if (tail) { try { fs.writeSync(out, `[${Date.now()}] ${tail}\n`); } catch { /* closed */ } } });
  }
  proc.on('close', () => { try { fs.closeSync(out); } catch { /* already */ } });
  let exited = null; proc.on('exit', (code) => { exited = { code }; });
  const shownArgs = args.map((a, i) => (args[i - 1] === '--otp' ? '<otp>' : args[i - 1] === '--session-id' ? '<session>' : a));
  const t0 = Date.now(); let ip = null; let log = '';
  const portRe = new RegExp(`https?://(\\d+\\.\\d+\\.\\d+\\.\\d+):(?:${o.httpsPort}|${o.httpPort})`);
  while (Date.now() - t0 < 40000) {
    await sleep(300);
    try { log = fs.readFileSync(o.logFile, 'utf8'); } catch { /* not yet */ }
    if (!ip) { const m = log.match(portRe); if (m) ip = m[1]; }
    if (ip) {
      try { const r = await fetch(`http://${ip}:${o.httpPort}/health`, { signal: AbortSignal.timeout(1500) }); if (r.ok) break; } catch { /* not up */ }
    }
    if (exited) break;
  }
  const up = !!ip && !exited;
  return { proc, pid: proc.pid, args: shownArgs, credentialsVia, ip, up, startupMs: Date.now() - t0, exited: () => exited, logTail: up ? null : log.slice(-3000) };
}

/** Stop a host: the `stop` verb where the build can be reached by it, else by PID. */
export async function stopHost(h, { exe, env, useStopVerb = true }) {
  const res = { method: null, stopVerb: null, pidKilled: false, exitedCleanly: false };
  if (!h || !h.pid) return res;
  if (!h.exited() && useStopVerb) {
    const r = await runCli(exe, ['stop'], { env, timeout: 15000 });
    res.stopVerb = { code: r.code, out: (r.stdout + r.stderr).trim().slice(0, 300) };
    for (let i = 0; i < 60 && !h.exited(); i++) await sleep(250);
    if (h.exited()) { res.method = 'stop verb'; res.exitedCleanly = true; }
  } else if (h.exited()) res.method = 'already exited';
  if (!h.exited() && pidAlive(h.pid)) {
    try { process.kill(h.pid); res.pidKilled = true; res.method = 'PID kill (the stop verb did not reach this host)'; } catch (e) { res.killError = e.message; }
    for (let i = 0; i < 40 && pidAlive(h.pid); i++) await sleep(250);
  }
  res.stillRunning = pidAlive(h.pid);
  return res;
}

export function osLoad() { return os.cpus().map((c) => ({ ...c.times })); }
