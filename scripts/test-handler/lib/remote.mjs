// A browser on another machine, used only as a client: started headless over SSH with a fresh
// profile under one new directory in its /tmp, driven over CDP through an SSH port forward, and
// removed afterwards. Nothing else on that machine is read or written.
import { spawn, execFile } from 'node:child_process';
import crypto from 'node:crypto';
import net from 'node:net';
import { sleep } from './cdp.mjs';

export const SSH = process.env.SE_TEST_SSH || 'C:/Windows/System32/OpenSSH/ssh.exe';
const MAC_CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const q = (s) => `'${String(s).replace(/'/g, `'\\''`)}'`;

export function ssh(target, cmd, timeout = 30000) {
  return new Promise((resolve) => {
    execFile(SSH, ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8', target, cmd], { timeout, windowsHide: true, encoding: 'utf8' }, (err, stdout, stderr) => {
      resolve({ code: err ? (err.code ?? 1) : 0, out: (stdout || '').trim(), err: (stderr || '').trim() });
    });
  });
}

function freePort() {
  return new Promise((res, rej) => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => res(p)); }); s.on('error', rej); });
}

/** What the far machine offers as a client. Read-only. */
export async function probeRemote(target, { chrome = MAC_CHROME } = {}) {
  const r = await ssh(target, `uname -s; sw_vers -productVersion 2>/dev/null; ${q(chrome)} --version 2>/dev/null || echo no-chrome; ls -d /Applications/Firefox.app /Applications/Safari.app 2>/dev/null`);
  if (r.code !== 0) return { reachable: false, error: r.err || `ssh exit ${r.code}` };
  const lines = r.out.split('\n');
  return { reachable: true, os: lines[0], osVersion: lines[1], chrome: lines[2], otherBrowsers: lines.slice(3) };
}

/**
 * Start a headless Chrome on `target` and forward its debugging port here. Returns
 * { port (local), dir, cleanup() }. cleanup kills that Chrome by its profile path, removes the
 * directory and the tunnel, and reports anything still running.
 */
export async function startRemoteChrome(target, { chrome = MAC_CHROME, flags = [], log = () => {} } = {}) {
  const id = crypto.randomBytes(4).toString('hex');
  const dir = `/tmp/se-test-client-${id}`;
  const rport = 9300 + (crypto.randomBytes(1)[0] % 200);
  const args = ['--headless=new', `--remote-debugging-port=${rport}`, `--user-data-dir=${dir}/profile`, '--ignore-certificate-errors', '--autoplay-policy=no-user-gesture-required',
    '--use-fake-device-for-media-stream', '--mute-audio', '--no-first-run', '--no-default-browser-check',
    '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows', ...flags, 'about:blank'];
  const start = await ssh(target, `mkdir ${q(dir)} && (nohup ${q(chrome)} ${args.map(q).join(' ')} > ${q(dir + '/chrome.log')} 2>&1 & echo $!)`);
  if (start.code !== 0) throw new Error('remote chrome did not start: ' + (start.err || start.out));
  const lport = await freePort();
  const tunnel = spawn(SSH, ['-o', 'BatchMode=yes', '-o', 'ExitOnForwardFailure=yes', '-N', '-L', `${lport}:127.0.0.1:${rport}`, target], { stdio: 'ignore', windowsHide: true });
  let tunnelExited = false; tunnel.on('exit', () => { tunnelExited = true; });
  log(`remote chrome on ${target}: pid ${start.out}, debugging ${rport} -> local ${lport}`);
  await sleep(500);
  const cleanup = async () => {
    const k = await ssh(target, `pkill -f ${q(dir + '/profile')}; sleep 1; pkill -9 -f ${q(dir + '/profile')}; rm -rf ${q(dir)}; echo LEFT=$(pgrep -f ${q(dir + '/profile')} | tr '\\n' ' ')`);
    try { tunnel.kill(); } catch { /* gone */ }
    for (let i = 0; i < 20 && !tunnelExited; i++) await sleep(100);
    const left = ((k.out.match(/LEFT=(.*)/) || [])[1] || '').trim();
    return { dirRemoved: k.code === 0, left: left || null, tunnelClosed: tunnelExited };
  };
  return { port: lport, dir, remotePid: start.out, cleanup, get tunnelExited() { return tunnelExited; } };
}
