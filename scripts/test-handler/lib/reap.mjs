// What a handler that was killed outright leaves behind: its host, its browsers, its virtual
// display and the display lock. Everything here is matched to a handler or host pid that is no
// longer alive, so a running test is never touched.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pidAlive } from './slots.mjs';
import { driverState, driverRemove } from './driver.mjs';

function processes(name) {
  try {
    const out = execFileSync('powershell', ['-NoProfile', '-Command', `Get-CimInstance Win32_Process -Filter "Name='${name}'" | Select-Object ProcessId,ParentProcessId,CommandLine | ConvertTo-Json -Compress`], { encoding: 'utf8', windowsHide: true, timeout: 30000 }).trim();
    if (!out) return [];
    const v = JSON.parse(out); return Array.isArray(v) ? v : [v];
  } catch { return []; }
}
function kill(pid) { try { process.kill(pid, 'SIGKILL'); return true; } catch { return false; } }

export async function reap({ log = () => {}, displayLock = process.env.SE_TEST_DISPLAY_LOCK, gate = async () => null } = {}) {
  const out = { browsers: [], hosts: [], monitors: [], displayLock: null };
  // browsers: the handler names each profile ...profile-slot<n>-<offset>-<handler pid>-<seq>
  for (const exe of ['chrome.exe', 'msedge.exe']) {
    for (const p of processes(exe)) {
      const m = /profile-slot\d+-\d+-(\d+)-\d+/.exec(p.CommandLine || '');
      if (m && !pidAlive(Number(m[1])) && !/--type=/.test(p.CommandLine)) { if (kill(p.ProcessId)) out.browsers.push(p.ProcessId); }
    }
  }
  // hosts: a `serve` on a slot port whose parent (the handler) is gone
  for (const p of processes('ScreenExtend.exe').concat(processes('ScreenExtend-bench-foundation.exe'))) {
    const c = p.CommandLine || '';
    if (/\bserve\b/.test(c) && /--http-port 180[1-4]0\b/.test(c) && !pidAlive(p.ParentProcessId)) { await gate('reap: kill an orphaned host'); if (kill(p.ProcessId)) out.hosts.push(p.ProcessId); }
  }
  // monitors: shared-driver mode records each monitor's owning host pid; removing one from the
  // middle of the id run detaches every display, so only when no live host owns any
  const ownersFile = path.join(os.tmpdir(), 'screenextend-shared-driver', 'owners.json');
  let owners = {}; try { owners = JSON.parse(fs.readFileSync(ownersFile, 'utf8')); } catch { /* none */ }
  const st = await driverState();
  if (Array.isArray(st) && st.length) {
    const live = st.filter((m) => owners[m.id] && pidAlive(owners[m.id]));
    const ours = st.filter((m) => (m.name || '').startsWith('ScreenExtend - SEH'));
    if (!live.length && ours.length === st.length) {
      await gate('reap: remove orphaned monitors');
      await driverRemove(ours.map((m) => m.id)).catch((e) => { out.monitorError = e.message; });
      out.monitors = ours.map((m) => ({ id: m.id, name: m.name, owner: owners[m.id] || null }));
      for (const m of ours) delete owners[m.id];
      try { fs.writeFileSync(ownersFile, JSON.stringify(owners)); } catch { /* best effort */ }
    } else if (ours.length > live.length) out.monitorsSkipped = { reason: 'a live host owns a monitor; removing one from the middle would detach every display', live: live.map((m) => m.id), all: st.map((m) => m.id) };
  }
  // the display lock, when the handler that wrote it is gone
  if (displayLock && fs.existsSync(displayLock)) {
    let owner = ''; try { owner = fs.readFileSync(path.join(displayLock, 'owner.txt'), 'utf8'); } catch { /* none */ }
    const m = /test-handler pid (\d+)/.exec(owner);
    if (m && !pidAlive(Number(m[1]))) { try { fs.rmSync(path.join(displayLock, 'owner.txt'), { force: true }); fs.rmdirSync(displayLock); out.displayLock = 'removed (owner pid ' + m[1] + ' gone)'; } catch (e) { out.displayLock = 'could not remove: ' + e.message; } }
  }
  if (out.browsers.length || out.hosts.length || out.monitors.length || out.displayLock) log('reaped: ' + JSON.stringify(out));
  return out;
}
