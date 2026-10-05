// Machine load: whole-machine CPU from os.cpus() deltas, GPU from nvidia-smi. Sampled once a
// second while a window is open, so a noisy run can be recognised afterwards.
import { execFile } from 'node:child_process';
import os from 'node:os';

function cpuTotals() {
  let idle = 0, total = 0;
  for (const c of os.cpus()) { const t = c.times; idle += t.idle; total += t.user + t.nice + t.sys + t.idle + t.irq; }
  return { idle, total };
}

function nvidiaSmi() {
  return new Promise((resolve) => {
    execFile('nvidia-smi', ['--query-gpu=utilization.gpu,utilization.encoder,utilization.decoder,memory.used,clocks.gr,temperature.gpu,power.draw', '--format=csv,noheader,nounits'],
      { timeout: 4000, windowsHide: true }, (err, out) => {
        if (err) return resolve({ error: err.code === 'ENOENT' ? 'nvidia-smi not found' : err.message });
        const f = out.trim().split('\n')[0].split(',').map((x) => parseFloat(x));
        resolve({ gpu: f[0], enc: f[1], dec: f[2], memMiB: f[3], clockMHz: f[4], tempC: f[5], powerW: f[6] });
      });
  });
}

export function startLoadSampler(periodMs = 1000) {
  const samples = []; let last = cpuTotals();
  const timer = setInterval(async () => {
    const cur = cpuTotals(); const g = await nvidiaSmi();
    const dt = cur.total - last.total, di = cur.idle - last.idle; last = cur;
    samples.push({ t: Date.now(), cpuPct: dt > 0 ? +(100 * (1 - di / dt)).toFixed(1) : null, ...g });
  }, periodMs);
  return { stop() { clearInterval(timer); return summarize(samples); } };
}

export async function snapshot() {
  const a = cpuTotals(); await new Promise((r) => setTimeout(r, 500)); const b = cpuTotals();
  return { cpuPct: +(100 * (1 - (b.idle - a.idle) / (b.total - a.total))).toFixed(1), ...(await nvidiaSmi()) };
}

function summarize(s) {
  const pick = (k) => s.map((x) => x[k]).filter((v) => typeof v === 'number' && !Number.isNaN(v));
  const st = (k) => { const v = pick(k); if (!v.length) return null; return { mean: +(v.reduce((a, b) => a + b, 0) / v.length).toFixed(1), max: Math.max(...v) }; };
  const err = s.find((x) => x.error);
  return { samples: s.length, cpuPct: st('cpuPct'), gpuPct: st('gpu'), encoderPct: st('enc'), decoderPct: st('dec'), gpuClockMHz: st('clockMHz'), error: err ? err.error : undefined };
}
