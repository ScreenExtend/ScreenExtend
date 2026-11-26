# Read-only probe of which top-level windows answer messages. Every 250 ms it sends WM_NULL to
# every top-level window on the desktop (SendMessageTimeout, SMTO_ABORTIFHUNG, -TimeoutMs) and
# prints the ones that did not answer in time, or took -SlowMs or longer:
#   {"hp":<epoch-ms>,"n":<windows probed>,"slow":[[hwnd,pid,tid,process,class,title,visible,hung,ms]]}
# ms is the answer time, or -1 for no answer within the timeout. "tray" lists Explorer's taskbar
# windows (Shell_TrayWnd, Shell_SecondaryTrayWnd) with their rectangles, so a new monitor's taskbar
# can be timed. Once a second, for -HostPid, it also prints every thread of that process that owns
# a window, message-only windows included:
#   {"hw":<epoch-ms>,"threads":{"<tid>":["<class>", ...]}}
# It sends nothing but WM_NULL, moves nothing and takes no focus.
param([int]$Seconds = 60, [int]$HostPid = 0, [int]$TimeoutMs = 250, [int]$SlowMs = 100)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Collections.Concurrent;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading.Tasks;
public static class Hang {
  delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr FindWindowExW(IntPtr parent, IntPtr after, string cls, string title);
  [DllImport("user32.dll")] static extern IntPtr SendMessageTimeoutW(IntPtr h, uint msg, IntPtr w, IntPtr l, uint flags, uint timeout, out IntPtr result);
  [DllImport("user32.dll")] static extern bool IsHungAppWindow(IntPtr h);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [StructLayout(LayoutKind.Sequential)] struct RECT { public int l, t, r, b; }
  [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
  static readonly IntPtr HWND_MESSAGE = new IntPtr(-3);
  static readonly ConcurrentDictionary<uint, string> names = new ConcurrentDictionary<uint, string>();
  static string Esc(string s) { var b = new StringBuilder(); foreach (char c in s ?? "") { if (c == '"' || c == '\\') b.Append('\\').Append(c); else if (c < 32) b.Append(' '); else b.Append(c); } return b.ToString(); }
  static string Name(uint pid) { return names.GetOrAdd(pid, p => { try { return Process.GetProcessById((int)p).ProcessName; } catch { return "?"; } }); }
  static string Cls(IntPtr h) { var s = new StringBuilder(256); GetClassNameW(h, s, 256); return s.ToString(); }
  static string Title(IntPtr h) { var s = new StringBuilder(128); GetWindowTextW(h, s, 128); return s.ToString(); }
  public static string Sample(uint timeoutMs, int slowMs) {
    var hs = new List<IntPtr>();
    EnumWindows((h, l) => { hs.Add(h); return true; }, IntPtr.Zero);
    var slow = new ConcurrentBag<string>(); var tray = new List<string>();
    foreach (var h in hs) { var c = Cls(h); if (c == "Shell_TrayWnd" || c == "Shell_SecondaryTrayWnd") { RECT rc; GetWindowRect(h, out rc); tray.Add(string.Format("[{0},\"{1}\",{2},{3},{4},{5},{6}]", h.ToInt64(), c, rc.l, rc.t, rc.r, rc.b, IsWindowVisible(h) ? "true" : "false")); } }
    Parallel.ForEach(hs, new ParallelOptions { MaxDegreeOfParallelism = 64 }, h => {
      var sw = Stopwatch.StartNew(); IntPtr r;
      IntPtr ok = SendMessageTimeoutW(h, 0 /* WM_NULL */, IntPtr.Zero, IntPtr.Zero, 0x0002 /* SMTO_ABORTIFHUNG */, timeoutMs, out r);
      long ms = sw.ElapsedMilliseconds;
      if (ok == IntPtr.Zero || ms >= slowMs) {
        uint pid; uint tid = GetWindowThreadProcessId(h, out pid);
        slow.Add(string.Format("[{0},{1},{2},\"{3}\",\"{4}\",\"{5}\",{6},{7},{8}]", h.ToInt64(), pid, tid, Esc(Name(pid)), Esc(Cls(h)), Esc(Title(h)), IsWindowVisible(h) ? "true" : "false", IsHungAppWindow(h) ? "true" : "false", ok == IntPtr.Zero ? -1 : ms));
      }
    });
    return "{\"n\":" + hs.Count + ",\"slow\":[" + string.Join(",", slow) + "],\"tray\":[" + string.Join(",", tray) + "]}";
  }
  public static string HostThreads(uint hostPid) {
    var map = new SortedDictionary<uint, List<string>>();
    Action<IntPtr, string> add = (h, tag) => { uint pid; uint tid = GetWindowThreadProcessId(h, out pid); if (pid != hostPid) return; List<string> l; if (!map.TryGetValue(tid, out l)) map[tid] = l = new List<string>(); l.Add(tag + Cls(h)); };
    EnumWindows((h, l) => { add(h, IsWindowVisible(h) ? "" : "(hidden)"); return true; }, IntPtr.Zero);
    IntPtr m = IntPtr.Zero; int guard = 0;
    while ((m = FindWindowExW(HWND_MESSAGE, m, null, null)) != IntPtr.Zero && guard++ < 10000) add(m, "(message-only)");
    var parts = new List<string>();
    foreach (var kv in map) parts.Add("\"" + kv.Key + "\":[" + string.Join(",", kv.Value.ConvertAll(c => "\"" + Esc(c) + "\"")) + "]");
    return "{" + string.Join(",", parts) + "}";
  }
}
'@
$epoch = [DateTime]::new(1970, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
function NowMs { [long]([DateTime]::UtcNow - $epoch).TotalMilliseconds }
$sw = [Diagnostics.Stopwatch]::StartNew(); $lastHost = -1000
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
  $t = NowMs; $s = [Hang]::Sample([uint32]$TimeoutMs, $SlowMs)
  "{""hp"":$t," + $s.Substring(1)
  if ($HostPid -gt 0 -and $sw.ElapsedMilliseconds - $lastHost -ge 1000) { $lastHost = $sw.ElapsedMilliseconds; "{""hw"":$(NowMs),""threads"":$([Hang]::HostThreads([uint32]$HostPid))}" }
  [Console]::Out.Flush()
  $rest = 250 - ((NowMs) - $t); if ($rest -gt 0) { Start-Sleep -Milliseconds $rest }
}
