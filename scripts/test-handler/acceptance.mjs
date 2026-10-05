#!/usr/bin/env node
// TEST-WORKFLOW.md §5.7 acceptance for the handler itself.
//
//   node scripts/test-handler/acceptance.mjs concurrent --exe <isolated build> [--n 5] [--request <json>]
//   node scripts/test-handler/acceptance.mjs noise      --exe <build> [--runs 5] [--request <json>]
//   node scripts/test-handler/acceptance.mjs exclusive  --exe <build without the switch> [--request <json>]
//
// Prints one JSON summary; each handler run's full document is in its own runDir.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ROOT } from './lib/slots.mjs';
import { spread } from './lib/metrics.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const argv = process.argv.slice(2);
const mode = argv[0];
const opt = (k, d) => { const i = argv.indexOf('--' + k); return i >= 0 ? argv[i + 1] : d; };
const exe = opt('exe');
const request = opt('request', '{}');

function runHandler(req, label) {
  return new Promise((resolve) => {
    const t0 = Date.now();
    const p = spawn(process.execPath, [path.join(HERE, 'handler.mjs'), '--exe', exe, '--request', JSON.stringify({ ...JSON.parse(request), ...req, label })], { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '', err = '';
    p.stdout.on('data', (d) => { out += d; }); p.stderr.on('data', (d) => { err += d; process.stderr.write(`[${label}] ${d}`); });
    p.on('exit', (code) => { let r = null; try { r = JSON.parse(out); } catch { /* none */ } resolve({ label, code, ms: Date.now() - t0, startedAt: t0, result: r, stderrTail: err.slice(-1500) }); });
  });
}

function summarize(x) {
  const r = x.result || {};
  const sc = r.scenarios || {};
  const lat = {};
  for (const [k, v] of Object.entries(sc)) if (v && v.runs) lat[k] = v.runs.map((w) => (w.error ? { error: w.error } : { p50: w.latencyMs.p50, p90: w.latencyMs.p90, p99: w.latencyMs.p99, max: w.latencyMs.max, fps: w.paintedFps, gap: w.longestGapMs, cpu: w.load && w.load.cpuPct ? w.load.cpuPct.mean : null }));
  return { label: x.label, exit: x.code, wallMs: x.ms, ok: r.ok, mode: r.mode, slot: r.slot, queueWaitMs: r.queueWaitMs, errors: r.errors, joins: (r.joins || []).map((j) => ({ answerMs: j.answerMs, firstPictureMs: j.firstPictureMs, offers: j.offers })), latency: lat, static: sc.static ? { framesWithChange: sc.static.framesWithChange, paintedFps: sc.static.paintedFps, greenFrames: sc.static.greenFrames } : undefined, cleanup: r.cleanup ? { displaysLeft: r.cleanup.displaysLeftByUs, hostsLeft: r.cleanup.hostProcessesLeft, chromeLeft: r.cleanup.chromeLeft, driverRemoved: r.cleanup.driverRemoved, hostStop: (r.cleanup.hosts || []).map((h) => h.method) } : null, ownerConfigIdentical: r.ownerConfig ? r.ownerConfig.identical : null, host: r.host ? { credentialsVia: r.host.credentialsVia, statusJson: r.host.statusJson, controlVerbs: r.host.controlVerbs, isolationVerified: r.host.isolationVerified } : null, warnings: r.warnings, runDir: r.runDir, windows: r.measurementWindows };
}

/** Every timed window from every run, checked for overlap. */
function overlapCheck(runs) {
  const w = runs.flatMap((x) => ((x.result && x.result.measurementWindows) || []).map((m) => ({ ...m, run: x.label })));
  w.sort((a, b) => a.start - b.start);
  const overlaps = [];
  for (let i = 1; i < w.length; i++) if (w[i].start < w[i - 1].end) overlaps.push({ a: `${w[i - 1].run}:${w[i - 1].label}`, b: `${w[i].run}:${w[i].label}`, byMs: w[i - 1].end - w[i].start });
  return { windows: w.length, overlaps };
}

let out;
if (mode === 'concurrent') {
  const n = +opt('n', 5);
  const runs = await Promise.all(Array.from({ length: n }, (_, i) => new Promise((r) => setTimeout(r, i * 1500)).then(() => runHandler({}, `c${i + 1}`))));
  const sum = runs.map(summarize);
  const timeline = sum.map((s) => ({ label: s.label, slot: s.slot, queueWaitMs: s.queueWaitMs, startedAt: runs.find((r) => r.label === s.label).startedAt }));
  out = { mode, n, runs: sum, timeline, serialisation: overlapCheck(runs), queued: sum.filter((s) => s.queueWaitMs > 2000).map((s) => s.label), allCompleted: sum.every((s) => s.exit === 0 && s.ok) };
} else if (mode === 'noise') {
  const n = +opt('runs', 5); const runs = [];
  for (let i = 0; i < n; i++) runs.push(await runHandler({}, `n${i + 1}`));
  const sum = runs.map(summarize);
  const per = {};
  for (const s of sum) for (const [k, ws] of Object.entries(s.latency)) for (const w of ws) { if (w.error) continue; (per[k] = per[k] || { p50: [], p90: [], p99: [], max: [], fps: [], gap: [] }); for (const f of Object.keys(per[k])) per[k][f].push(w[f]); }
  const noise = {}; for (const [k, v] of Object.entries(per)) { noise[k] = {}; for (const [f, vals] of Object.entries(v)) noise[k][f] = spread(vals); }
  const joins = sum.flatMap((s) => s.joins);
  noise.join = { answerMs: spread(joins.map((j) => j.answerMs)), firstPictureMs: spread(joins.map((j) => j.firstPictureMs)) };
  out = { mode, runs: sum, noise };
} else if (mode === 'exclusive') {
  const runs = [await runHandler({}, 'x1')];
  out = { mode, runs: runs.map(summarize) };
} else { process.stderr.write('usage: acceptance.mjs concurrent|noise|exclusive --exe <path>\n'); process.exit(2); }
out.finishedAt = new Date().toISOString();
const file = path.join(ROOT, `acceptance-${mode}-${Date.now()}.json`);
fs.writeFileSync(file, JSON.stringify(out, null, 1));
out.file = file;
process.stdout.write(JSON.stringify(out, null, 1) + '\n');
