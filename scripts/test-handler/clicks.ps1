# A train of short tone bursts on the default output, each at a recorded host time.
#
# winmm waveOut, 48 kHz mono 16-bit, streamed in 10 ms blocks. The host time of a click is the
# moment waveOutGetPosition says the device path consumed the click's first sample (interpolated
# between position samples taken every block), in Unix milliseconds on the system clock. That is
# the same reference for every build under test, so delays compare; it is not "the instant the
# speaker moved". Prints one JSON document at the end: {"clicks":[ms,...], ...}.
param([int]$Seconds = 12, [int]$IntervalMs = 700, [double]$Amplitude = 0.25, [int]$BurstMs = 4, [int]$FreqHz = 2000)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;

public static class Clicks {
  [StructLayout(LayoutKind.Sequential)] struct WAVEFORMATEX { public ushort wFormatTag, nChannels; public uint nSamplesPerSec, nAvgBytesPerSec; public ushort nBlockAlign, wBitsPerSample, cbSize; }
  [StructLayout(LayoutKind.Sequential)] struct WAVEHDR { public IntPtr lpData; public uint dwBufferLength, dwBytesRecorded; public IntPtr dwUser; public uint dwFlags, dwLoops; public IntPtr lpNext, reserved; }
  [StructLayout(LayoutKind.Sequential)] struct MMTIME { public uint wType; public uint u; public uint pad; }
  [DllImport("winmm.dll")] static extern int waveOutOpen(out IntPtr h, int dev, ref WAVEFORMATEX f, IntPtr cb, IntPtr inst, uint flags);
  [DllImport("winmm.dll")] static extern int waveOutPrepareHeader(IntPtr h, IntPtr hdr, int size);
  [DllImport("winmm.dll")] static extern int waveOutUnprepareHeader(IntPtr h, IntPtr hdr, int size);
  [DllImport("winmm.dll")] static extern int waveOutWrite(IntPtr h, IntPtr hdr, int size);
  [DllImport("winmm.dll")] static extern int waveOutGetPosition(IntPtr h, ref MMTIME t, int size);
  [DllImport("winmm.dll")] static extern int waveOutReset(IntPtr h);
  [DllImport("winmm.dll")] static extern int waveOutClose(IntPtr h);
  [DllImport("winmm.dll")] static extern uint timeBeginPeriod(uint p);

  const int RATE = 48000, BLOCK = 480, NBUF = 6;
  static readonly DateTime Epoch = new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc);
  static double NowMs() { return (DateTime.UtcNow - Epoch).TotalMilliseconds; }

  public static string Run(int seconds, int intervalMs, double amp, int burstMs, int freq) {
    timeBeginPeriod(1);
    var f = new WAVEFORMATEX { wFormatTag = 1, nChannels = 1, nSamplesPerSec = RATE, nAvgBytesPerSec = RATE * 2, nBlockAlign = 2, wBitsPerSample = 16 };
    IntPtr h; int r = waveOutOpen(out h, -1, ref f, IntPtr.Zero, IntPtr.Zero, 0);
    if (r != 0) return "{\"error\":\"waveOutOpen " + r + "\"}";
    int hsz = Marshal.SizeOf(typeof(WAVEHDR));
    var hdrs = new IntPtr[NBUF]; var bufs = new IntPtr[NBUF];
    for (int i = 0; i < NBUF; i++) {
      bufs[i] = Marshal.AllocHGlobal(BLOCK * 2); hdrs[i] = Marshal.AllocHGlobal(hsz);
      var wh = new WAVEHDR { lpData = bufs[i], dwBufferLength = BLOCK * 2 };
      Marshal.StructureToPtr(wh, hdrs[i], false);
      waveOutPrepareHeader(h, hdrs[i], hsz);
    }
    long total = (long)seconds * RATE, written = 0;
    var clickAt = new List<long>(); // sample index of each click onset
    for (long s = RATE; s + RATE / 2 < total; s += (long)intervalMs * RATE / 1000) clickAt.Add(s);
    int burst = RATE * burstMs / 1000;
    var posT = new List<double>(); var posS = new List<long>();
    var tmp = new short[BLOCK];
    Func<int, bool> fill = (i) => {
      if (written >= total) return false;
      for (int k = 0; k < BLOCK; k++) {
        long s = written + k; double v = 0;
        foreach (var c in clickAt) { long d = s - c; if (d >= 0 && d < burst) { v = amp * Math.Sin(2 * Math.PI * freq * d / RATE); break; } }
        tmp[k] = (short)(v * 32767);
      }
      Marshal.Copy(tmp, 0, bufs[i], BLOCK);
      var wh = (WAVEHDR)Marshal.PtrToStructure(hdrs[i], typeof(WAVEHDR));
      wh.dwFlags &= ~1u; // clear WHDR_DONE
      Marshal.StructureToPtr(wh, hdrs[i], false);
      waveOutWrite(h, hdrs[i], hsz); written += BLOCK; return true;
    };
    for (int i = 0; i < NBUF; i++) fill(i);
    var mt = new MMTIME();
    long lastPos = -1;
    while (true) {
      bool allDone = true;
      for (int i = 0; i < NBUF; i++) {
        var wh = (WAVEHDR)Marshal.PtrToStructure(hdrs[i], typeof(WAVEHDR));
        if ((wh.dwFlags & 1) != 0) { if (fill(i)) allDone = false; }
        else allDone = false;
      }
      mt.wType = 2; /* TIME_SAMPLES */
      double t0 = NowMs(); waveOutGetPosition(h, ref mt, Marshal.SizeOf(typeof(MMTIME))); double t1 = NowMs();
      long pos = mt.u;
      if (pos != lastPos) { posT.Add((t0 + t1) / 2); posS.Add(pos); lastPos = pos; }
      if (allDone && written >= total) break;
      if (pos >= total) break;
      Thread.Sleep(1);
    }
    waveOutReset(h);
    for (int i = 0; i < NBUF; i++) { waveOutUnprepareHeader(h, hdrs[i], hsz); Marshal.FreeHGlobal(hdrs[i]); Marshal.FreeHGlobal(bufs[i]); }
    waveOutClose(h);
    // Each click: the first position sample at or past its onset, back-projected at the sample rate.
    var sb = new System.Text.StringBuilder("{\"clicks\":[");
    int j = 0; bool first = true; int unresolved = 0;
    foreach (var c in clickAt) {
      while (j < posS.Count && posS[j] < c) j++;
      if (j >= posS.Count) { unresolved++; continue; }
      double t = posT[j] - (posS[j] - c) * 1000.0 / RATE;
      sb.Append(first ? "" : ",").Append(t.ToString("F2", System.Globalization.CultureInfo.InvariantCulture)); first = false;
    }
    sb.Append("],\"unresolved\":" + unresolved + ",\"positionSamples\":" + posS.Count + ",\"intervalMs\":" + intervalMs + ",\"burstMs\":" + burstMs + ",\"freqHz\":" + freq + ",\"amplitude\":" + amp.ToString(System.Globalization.CultureInfo.InvariantCulture) + "}");
    return sb.ToString();
  }
}
'@
[Clicks]::Run($Seconds, $IntervalMs, $Amplitude, $BurstMs, $FreqHz)
