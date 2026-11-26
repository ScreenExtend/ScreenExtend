// Runs a program with its stdout and stderr read by this process alone, each line written to
// <logfile> as "[epoch-ms] line". The handler's own event loop can stall (synchronous calls,
// a busy machine); a host writing a verbose log into a pipe nobody is reading blocks on that
// write, so the reader lives in a process that does nothing else.
//
//   node tslog.mjs <logfile> <exe> [args...]
// Prints {"pid": <child pid>} on stdout once the child has started; exits with the child's code.
import { spawn } from 'node:child_process';
import fs from 'node:fs';

const [, , logFile, exe, ...args] = process.argv;
const out = fs.openSync(logFile, 'a');
const child = spawn(exe, args, { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
process.stdout.write(JSON.stringify({ pid: child.pid }) + '\n');
for (const s of [child.stdout, child.stderr]) {
  let tail = '';
  s.setEncoding('utf8');
  s.on('data', (d) => {
    const now = Date.now();
    const parts = (tail + d).split('\n'); tail = parts.pop();
    if (parts.length) { try { fs.writeSync(out, parts.map((l) => `[${now}] ${l.replace(/\r$/, '')}\n`).join('')); } catch { /* closed */ } }
  });
  s.on('end', () => { if (tail) { try { fs.writeSync(out, `[${Date.now()}] ${tail}\n`); } catch { /* closed */ } } });
}
child.on('exit', (code) => { setTimeout(() => { try { fs.closeSync(out); } catch { /* closed */ } process.exit(code ?? 1); }, 200); });
