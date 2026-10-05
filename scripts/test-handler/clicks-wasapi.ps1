# A train of short tone bursts on the default output, timed by the audio engine's own clock.
#
# WASAPI shared mode on the default render endpoint, in the engine's mix format. Each click is
# written at a known stream frame, and `IAudioClock::GetPosition` reports, with a QPC stamp, which
# frame the device is playing. The host time of a click is therefore the moment the endpoint's
# play position reaches the click's first frame — the instant the host's own speaker plays it —
# converted to Unix milliseconds through one paired QPC/precise-system-time sample. Unlike the
# winmm train (clicks.ps1), nothing between this process and the engine is in the number: no
# winmm queue, no sleep-driven position polling.
#
# The loopback capture sees the mix a little *before* the play cursor reaches it (on this machine
# about 15 ms: see f36f2ec), so "client render − this time" is the client's lag behind the host's
# own speaker, which can be smaller than the host's capture → client render span. Prints one JSON
# document: {"clicks":[ms,...], ...} — the same shape as clicks.ps1.
param([int]$Seconds = 12, [int]$IntervalMs = 700, [double]$Amplitude = 0.25, [int]$BurstMs = 4, [int]$FreqHz = 2000)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;

[ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDeviceEnumerator {
  int EnumAudioEndpoints(int dataFlow, int mask, out IntPtr devices);
  int GetDefaultAudioEndpoint(int dataFlow, int role, out IMMDevice device);
}
[ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDevice {
  int Activate(ref Guid iid, int clsCtx, IntPtr activationParams, [MarshalAs(UnmanagedType.IUnknown)] out object iface);
}
[ComImport, Guid("1CB9AD4C-DBFA-4c32-B178-C2F568A703B2"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioClient {
  int Initialize(int shareMode, uint flags, long bufferDuration, long periodicity, IntPtr format, IntPtr sessionGuid);
  int GetBufferSize(out uint frames);
  int GetStreamLatency(out long latency);
  int GetCurrentPadding(out uint padding);
  int IsFormatSupported(int shareMode, IntPtr format, out IntPtr closest);
  int GetMixFormat(out IntPtr format);
  int GetDevicePeriod(out long defaultPeriod, out long minPeriod);
  int Start();
  int Stop();
  int Reset();
  int SetEventHandle(IntPtr h);
  int GetService(ref Guid iid, [MarshalAs(UnmanagedType.IUnknown)] out object service);
}
[ComImport, Guid("F294ACFC-3146-4483-A7BF-ADDCA7C260E2"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioRenderClient {
  int GetBuffer(uint frames, out IntPtr data);
  int ReleaseBuffer(uint frames, uint flags);
}
[ComImport, Guid("CD63314F-3FBA-4a1b-812C-EF96358728E7"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioClock {
  int GetFrequency(out ulong freq);
  int GetPosition(out ulong pos, out ulong qpcPos);
}
[ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")] class MMDeviceEnumeratorCo {}

public static class WasapiClicks {
  [DllImport("kernel32.dll")] static extern bool QueryPerformanceCounter(out long c);
  [DllImport("kernel32.dll")] static extern bool QueryPerformanceFrequency(out long f);
  [DllImport("kernel32.dll")] static extern void GetSystemTimePreciseAsFileTime(out long ft);
  const long UNIX_EPOCH_FT = 116444736000000000L;

  static void Check(int hr, string what) { if (hr < 0) throw new Exception(what + " failed: 0x" + hr.ToString("X8")); }

  public static string Run(int seconds, int intervalMs, double amp, int burstMs, int freq) {
    var en = (IMMDeviceEnumerator)new MMDeviceEnumeratorCo();
    IMMDevice dev; Check(en.GetDefaultAudioEndpoint(0 /*eRender*/, 0 /*eConsole*/, out dev), "GetDefaultAudioEndpoint");
    Guid iidClient = typeof(IAudioClient).GUID; object o;
    Check(dev.Activate(ref iidClient, 1 /*CLSCTX_INPROC_SERVER*/, IntPtr.Zero, out o), "Activate");
    var client = (IAudioClient)o;
    IntPtr pwfx; Check(client.GetMixFormat(out pwfx), "GetMixFormat");
    ushort tag = (ushort)Marshal.ReadInt16(pwfx, 0); int ch = Marshal.ReadInt16(pwfx, 2);
    int rate = Marshal.ReadInt32(pwfx, 4); int bits = Marshal.ReadInt16(pwfx, 14);
    // 3 = IEEE float; 0xFFFE = extensible, which the shared-mode mix format always is in float.
    if (bits != 32 || (tag != 3 && tag != 0xFFFE)) return "{\"error\":\"mix format is not 32-bit float (tag " + tag + ", bits " + bits + ")\"}";
    Check(client.Initialize(0 /*shared*/, 0, 200000 /*20 ms*/, 0, pwfx, IntPtr.Zero), "Initialize");
    uint bufFrames; Check(client.GetBufferSize(out bufFrames), "GetBufferSize");
    Guid iidRender = typeof(IAudioRenderClient).GUID, iidClock = typeof(IAudioClock).GUID;
    object so; Check(client.GetService(ref iidRender, out so), "GetService(render)"); var render = (IAudioRenderClient)so;
    Check(client.GetService(ref iidClock, out so), "GetService(clock)"); var clock = (IAudioClock)so;
    ulong clockFreq; Check(clock.GetFrequency(out clockFreq), "GetFrequency");

    // QPC (100 ns units, as GetPosition reports it) -> Unix ms, from the tightest of several pairs.
    long qf; QueryPerformanceFrequency(out qf);
    double best = double.MaxValue, offsetMs = 0;
    for (int i = 0; i < 50; i++) {
      long a, b, ft; QueryPerformanceCounter(out a); GetSystemTimePreciseAsFileTime(out ft); QueryPerformanceCounter(out b);
      double span = (b - a) * 1000.0 / qf;
      if (span < best) { best = span; double qpcMs = ((a + b) / 2.0) * 1000.0 / qf; offsetMs = (ft - UNIX_EPOCH_FT) / 10000.0 - qpcMs; }
    }

    long total = (long)seconds * rate, written = 0;
    var clickAt = new List<long>();
    for (long s = rate; s + rate / 2 < total; s += (long)intervalMs * rate / 1000) clickAt.Add(s);
    int burst = rate * burstMs / 1000;
    var posF = new List<double>(); var posT = new List<double>();
    var tmp = new float[bufFrames * ch];
    Func<uint, bool> fill = (frames) => {
      if (frames == 0 || written >= total) return false;
      IntPtr data; Check(render.GetBuffer(frames, out data), "GetBuffer");
      for (int k = 0; k < frames; k++) {
        long s = written + k; double v = 0;
        foreach (var c in clickAt) { long d = s - c; if (d >= 0 && d < burst) { v = amp * Math.Sin(2 * Math.PI * freq * d / rate); break; } }
        for (int cc = 0; cc < ch; cc++) tmp[k * ch + cc] = (float)v;
      }
      Marshal.Copy(tmp, 0, data, (int)frames * ch);
      Check(render.ReleaseBuffer(frames, 0), "ReleaseBuffer");
      written += frames; return true;
    };
    fill(bufFrames);
    Check(client.Start(), "Start");
    double lastF = -1;
    while (true) {
      uint pad; Check(client.GetCurrentPadding(out pad), "GetCurrentPadding");
      if (written < total) fill(bufFrames - pad);
      ulong pos, qpc; clock.GetPosition(out pos, out qpc);
      double f = (double)pos * rate / clockFreq;
      if (qpc != 0 && f != lastF) { posF.Add(f); posT.Add(qpc / 10000.0 + offsetMs); lastF = f; }
      if (written >= total && f >= total) break;
      Thread.Sleep(1);
    }
    client.Stop();
    Marshal.FreeCoTaskMem(pwfx);
    // Each click: the last position sample at or before its first frame, projected forward at the
    // sample rate (the device clock and the sample rate agree to well under a millisecond over a
    // few periods).
    var sb = new System.Text.StringBuilder("{\"clicks\":[");
    int j = 0; bool first = true; int unresolved = 0;
    foreach (var c in clickAt) {
      while (j + 1 < posF.Count && posF[j + 1] <= c) j++;
      if (posF.Count == 0 || posF[j] > c) { unresolved++; continue; }
      double t = posT[j] + (c - posF[j]) * 1000.0 / rate;
      sb.Append(first ? "" : ",").Append(t.ToString("F2", System.Globalization.CultureInfo.InvariantCulture)); first = false;
    }
    sb.Append("],\"source\":\"wasapi-shared IAudioClock play position\",\"rate\":" + rate + ",\"channels\":" + ch + ",\"bufferFrames\":" + bufFrames + ",\"unresolved\":" + unresolved + ",\"positionSamples\":" + posF.Count + ",\"qpcPairSpanMs\":" + best.ToString("F4", System.Globalization.CultureInfo.InvariantCulture) + ",\"intervalMs\":" + intervalMs + ",\"burstMs\":" + burstMs + ",\"freqHz\":" + freq + ",\"amplitude\":" + amp.ToString(System.Globalization.CultureInfo.InvariantCulture) + "}");
    return sb.ToString();
  }
}
'@
[WasapiClicks]::Run($Seconds, $IntervalMs, $Amplitude, $BurstMs, $FreqHz)
