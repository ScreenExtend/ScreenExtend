// The one reference clock every stamp and every browser time is compared on: the system's
// precise wall clock (GetSystemTimePreciseAsFileTime), in Unix milliseconds.
//
// Not Date.now() or performance.timeOrigin: at Windows' default timer resolution (in effect
// until some process asks for more) the coarse wall clock moves in 15.6 ms steps, so every clock
// anchored on it (node's Date.now(), each context's `performance.timeOrigin`, .NET's
// DateTime.UtcNow) is off by a different 0-16 ms per process: a run-to-run bias in the stamp.
//
// node's process.hrtime() is QueryPerformanceCounter on Windows, so one paired QPC / precise
// wall-clock sample taken in a helper process anchors it; after that preciseNow() is hrtime
// plus a constant.
import { execFileSync } from 'node:child_process';

let K = null;

function anchor() {
  const src = `Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public static class SeAnchor {
  [DllImport("kernel32.dll")] static extern bool QueryPerformanceCounter(out long c);
  [DllImport("kernel32.dll")] static extern bool QueryPerformanceFrequency(out long f);
  [DllImport("kernel32.dll")] static extern void GetSystemTimePreciseAsFileTime(out long ft);
  public static string Pair() {
    long f; QueryPerformanceFrequency(out f); double best = double.MaxValue, off = 0;
    for (int i = 0; i < 200; i++) { long a, b, ft; QueryPerformanceCounter(out a); GetSystemTimePreciseAsFileTime(out ft); QueryPerformanceCounter(out b);
      double span = (b - a) * 1000.0 / f; if (span < best) { best = span; off = (ft - 116444736000000000L) / 10000.0 - ((a + b) / 2.0) * 1000.0 / f; } }
    return off.ToString("R", System.Globalization.CultureInfo.InvariantCulture) + " " + best.ToString("R", System.Globalization.CultureInfo.InvariantCulture);
  }
}
"@
[SeAnchor]::Pair()`;
  const out = execFileSync('powershell.exe', ['-NoProfile', '-Command', src], { encoding: 'utf8', windowsHide: true }).trim().split(/\s+/);
  K = Number(out[0]);
  if (!Number.isFinite(K)) throw new Error('clock anchor failed: ' + out.join(' '));
  return { qpcToUnixMs: K, pairSpanMs: Number(out[1]) };
}

export let anchorInfo = null;
/** Precise Unix ms (QPC-anchored). */
export function preciseNow() {
  if (K === null) anchorInfo = anchor();
  return Number(process.hrtime.bigint()) / 1e6 + K;
}
