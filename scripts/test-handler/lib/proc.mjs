// Process counters for soak and churn: private bytes, handles, threads, sockets and GPU memory
// of the host, plus the display driver's host process (WUDFHost, which grows with every display
// created). One PowerShell call per sample.
import { execFile } from 'node:child_process';

export function procSample(pid) {
  const ps = [
    `$p = Get-Process -Id ${Number(pid) || 0} -ErrorAction SilentlyContinue`,
    "$w = Get-Process WUDFHost -ErrorAction SilentlyContinue | Sort-Object PrivateMemorySize64 -Descending | Select-Object -First 1",
    '$udp = 0; $tcp = 0; $gpu = $null',
    `if ($p) { $udp = @(Get-NetUDPEndpoint -OwningProcess ${Number(pid) || 0} -ErrorAction SilentlyContinue).Count; $tcp = @(Get-NetTCPConnection -OwningProcess ${Number(pid) || 0} -ErrorAction SilentlyContinue).Count`,
    `  try { $gpu = ((Get-Counter ('\\GPU Process Memory(pid_' + ${Number(pid) || 0} + '*)\\Dedicated Usage') -ErrorAction Stop).CounterSamples | Measure-Object CookedValue -Sum).Sum } catch { } }`,
    '[pscustomobject]@{ alive = [bool]$p; privateMB = if ($p) { [math]::Round($p.PrivateMemorySize64 / 1MB, 1) } else { $null }; workingSetMB = if ($p) { [math]::Round($p.WorkingSet64 / 1MB, 1) } else { $null };',
    '  handles = if ($p) { $p.HandleCount } else { $null }; threads = if ($p) { $p.Threads.Count } else { $null }; udpSockets = $udp; tcpSockets = $tcp; gpuDedicatedMB = if ($gpu -ne $null) { [math]::Round($gpu / 1MB, 1) } else { $null };',
    '  wudfPrivateMB = [math]::Round($w.PrivateMemorySize64 / 1MB, 1); wudfThreads = $w.Threads.Count; wudfHandles = $w.HandleCount } | ConvertTo-Json -Compress',
  ].join('\n');
  return new Promise((resolve) => {
    const t = Date.now();
    execFile('powershell', ['-NoProfile', '-Command', ps], { timeout: 30000, windowsHide: true, encoding: 'utf8' }, (err, out) => {
      if (err) return resolve({ t, error: err.message.slice(0, 200) });
      try { resolve({ t, ...JSON.parse(out.trim()) }); } catch { resolve({ t, error: 'parse: ' + out.slice(0, 200) }); }
    });
  });
}

/**
 * Least-squares slope of y over x (per hour when x is ms), with a 95 % interval from the
 * slope's standard error. `trend` says which way the interval points, or 'flat' when it spans 0.
 */
export function slope(points, perMs = 3600000) {
  const p = points.filter(([x, y]) => Number.isFinite(x) && Number.isFinite(y));
  const n = p.length;
  if (n < 4) return { n, slope: null };
  const mx = p.reduce((a, [x]) => a + x, 0) / n, my = p.reduce((a, [, y]) => a + y, 0) / n;
  let sxx = 0, sxy = 0; for (const [x, y] of p) { sxx += (x - mx) ** 2; sxy += (x - mx) * (y - my); }
  if (!sxx) return { n, slope: null };
  const b = sxy / sxx, a = my - b * mx;
  let sse = 0; for (const [x, y] of p) sse += (y - a - b * x) ** 2;
  const se = Math.sqrt(sse / (n - 2) / sxx);
  // t(0.975) for small n, 1.96 beyond 30 points
  const T = [12.71, 4.30, 3.18, 2.78, 2.57, 2.45, 2.36, 2.31, 2.26, 2.23, 2.20, 2.18, 2.16, 2.14, 2.13, 2.12, 2.11, 2.10, 2.09, 2.09, 2.08, 2.07, 2.07, 2.06, 2.06, 2.06, 2.05, 2.05];
  const t = n - 2 <= T.length ? T[n - 3] : 1.96;
  const k = (v) => +(v * perMs).toPrecision(4);
  const lo = (b - t * se) * perMs, hi = (b + t * se) * perMs;
  return { n, slope: k(b), ci95: [+lo.toPrecision(4), +hi.toPrecision(4)], trend: lo > 0 ? 'rising' : hi < 0 ? 'falling' : 'flat', start: +(a + b * p[0][0]).toPrecision(5), end: +(a + b * p[n - 1][0]).toPrecision(5) };
}
