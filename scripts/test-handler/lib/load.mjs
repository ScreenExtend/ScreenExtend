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
    execFile('nvidia-smi', ['--query-gpu=utilization.gpu,utilization.encoder,utilization.decoder,memory.used,clocks.gr,temperature.gpu,power.draw,clocks.video,pstate', '--format=csv,noheader,nounits'],
      { timeout: 4000, windowsHide: true }, (err, out) => {
        if (err) return resolve({ error: err.code === 'ENOENT' ? 'nvidia-smi not found' : err.message });
        const f = out.trim().split('\n')[0].split(',').map((x) => parseFloat(x));
        const raw = out.trim().split('\n')[0].split(',').map((x) => x.trim());
        resolve({ gpu: f[0], enc: f[1], dec: f[2], memMiB: f[3], clockMHz: f[4], tempC: f[5], powerW: f[6], videoClockMHz: f[7], pstate: raw[8] });
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

/**
 * The pre-window load gate. Runs at 85-100 % CPU came out 10-25 ms slower than quiet ones, so a
 * timed window waits (bounded) for the whole-machine CPU, averaged over `sampleMs`, to fall
 * below `maxCpu`. Returns what it saw; `loaded: true` means the bound ran out and the window
 * opened anyway, so the caller can discard it.
 */
export async function waitForQuiet({ maxCpu = 75, maxWaitMs = 180000, sampleMs = 2000 } = {}) {
  const t0 = Date.now(); const seen = [];
  for (;;) {
    const a = cpuTotals(); await new Promise((r) => setTimeout(r, sampleMs)); const b = cpuTotals();
    const cpu = +(100 * (1 - (b.idle - a.idle) / Math.max(1, b.total - a.total))).toFixed(1);
    seen.push(cpu);
    if (cpu < maxCpu) return { maxCpu, waitedMs: Date.now() - t0, cpuAtOpen: cpu, loaded: false, samples: seen.length };
    if (Date.now() - t0 >= maxWaitMs) return { maxCpu, waitedMs: Date.now() - t0, cpuAtOpen: cpu, loaded: true, samples: seen.length, minSeen: Math.min(...seen) };
  }
}

/**
 * Memory state: commit headroom, free physical memory, and the largest WUDFHost (the virtual
 * display driver's host process, which leaks with every display created). Taken at the start and
 * end of the live-display part of a run, under the measurement lock, so no other test creates a
 * display in between and the WUDFHost delta is this run's own.
 */
export function memSnapshot() {
  const ps = "$c=(Get-Counter '\\Memory\\Committed Bytes','\\Memory\\Commit Limit').CounterSamples; " +
    "$w = Get-Process WUDFHost -ErrorAction SilentlyContinue | Sort-Object PrivateMemorySize64 -Descending | Select-Object -First 1; " +
    "$os = Get-CimInstance Win32_OperatingSystem; " +
    "[pscustomobject]@{ commitHeadroomGB=[math]::Round(($c[1].CookedValue-$c[0].CookedValue)/1GB,2); freePhysGB=[math]::Round($os.FreePhysicalMemory/1MB,2); " +
    "wudfPid=$w.Id; wudfCommitMB=[math]::Round($w.PrivateMemorySize64/1MB,1); wudfHandles=$w.HandleCount; wudfThreads=$w.Threads.Count } | ConvertTo-Json -Compress; exit 0";
  return new Promise((resolve) => {
    execFile('powershell', ['-NoProfile', '-Command', ps], { timeout: 20000, windowsHide: true, encoding: 'utf8' }, (err, out) => {
      if (err) return resolve({ error: err.message });
      try { resolve({ t: Date.now(), ...JSON.parse(out.trim()) }); } catch (e) { resolve({ error: 'parse: ' + out.slice(0, 200) }); }
    });
  });
}

function summarize(s) {
  const pick = (k) => s.map((x) => x[k]).filter((v) => typeof v === 'number' && !Number.isNaN(v));
  const st = (k) => { const v = pick(k); if (!v.length) return null; return { mean: +(v.reduce((a, b) => a + b, 0) / v.length).toFixed(1), max: Math.max(...v) }; };
  const err = s.find((x) => x.error);
  return { samples: s.length, cpuPct: st('cpuPct'), gpuPct: st('gpu'), encoderPct: st('enc'), decoderPct: st('dec'), gpuClockMHz: st('clockMHz'), videoClockMHz: st('videoClockMHz'), powerW: st('powerW'), pstates: [...new Set(s.map((x) => x.pstate).filter(Boolean))], error: err ? err.error : undefined };
}
