// The one reference clock every stamp and every browser time is compared on: the system's
// precise wall clock (GetSystemTimePreciseAsFileTime), in Unix milliseconds.
//
// Why not Date.now() or performance.timeOrigin: at Windows' default 15.6 ms timer resolution
// (what a freshly booted machine runs at until some process asks for more) the coarse wall clock
// moves in 15.6 ms steps, and every clock anchored on it is off by a different 0-16 ms per
// process: node's Date.now(), node's and every Chrome context's `performance.timeOrigin`, and
// .NET's DateTime.UtcNow, which stage.ps1 used to draw the stamp with. Measured on 2026-10-06
// after a reboot: .NET UtcNow lagged the precise clock by a 0-15.9 ms sawtooth, node's
// timeOrigin was off by -3 to -8 ms per process, and the page's offset from node read 0.95 to
// 13.76 ms run to run — so the stamp metric carried a bias of up to about ±16 ms that changed
// from run to run. Before the reboot something on the machine held the timer at 1 ms and hid it.
//
// node's process.hrtime() is QueryPerformanceCounter on Windows, so one paired QPC / precise
// wall-clock sample taken in a helper process anchors it exactly; after that preciseNow() is
// hrtime plus a constant, immune to the coarse clock.
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
