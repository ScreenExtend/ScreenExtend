# One process's handles counted by object type, and its GPU memory per adapter. No elevation:
# NtQuerySystemInformation(SystemExtendedHandleInformation) lists every handle with its type
# index, and NtQueryObject(ObjectTypesInformation) names the indices.
#   -ProcessId N    prints one JSON line {types: {name: count}, total, gpu: [{adapter, dedicatedMB, sharedMB}]}
param([Parameter(Mandatory = $true)][int]$ProcessId)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System; using System.Collections.Generic; using System.Runtime.InteropServices;
public static class SeHandles {
  [DllImport("ntdll.dll")] static extern int NtQuerySystemInformation(int cls, IntPtr buf, int len, out int ret);
  [DllImport("ntdll.dll")] static extern int NtQueryObject(IntPtr h, int cls, IntPtr buf, int len, out int ret);
  static IntPtr Query(Func<IntPtr, int, int> call) {
    int len = 1 << 20;
    for (int i = 0; i < 12; i++) {
      IntPtr b = Marshal.AllocHGlobal(len);
      int st = call(b, len);
      if (st == 0) return b;
      Marshal.FreeHGlobal(b);
      if ((uint)st != 0xC0000004 && (uint)st != 0x80000005) throw new Exception("status 0x" + st.ToString("X"));
      len *= 2;
    }
    throw new Exception("buffer");
  }
  public static Dictionary<string, int> ByType(int pid, out int total) {
    // ObjectTypesInformation = 3: a count, then OBJECT_TYPE_INFORMATION records aligned to pointer size.
    var names = new Dictionary<int, string>();
    int r;
    IntPtr tb = Query((b, l) => NtQueryObject(IntPtr.Zero, 3, b, l, out r));
    try {
      int n = Marshal.ReadInt32(tb);
      IntPtr p = tb + IntPtr.Size;
      for (int i = 0; i < n; i++) {
        ushort nlen = (ushort)Marshal.ReadInt16(p); ushort nmax = (ushort)Marshal.ReadInt16(p, 2);
        IntPtr nbuf = Marshal.ReadIntPtr(p, IntPtr.Size);
        string name = Marshal.PtrToStringUni(nbuf, nlen / 2);
        names[Marshal.ReadByte(p, 90)] = name;
        int recSize = 16 + 88; // UNICODE_STRING (16) + fixed fields (88) on x64
        int adv = recSize + ((nmax + IntPtr.Size - 1) & ~(IntPtr.Size - 1));
        p = p + adv;
      }
    } finally { Marshal.FreeHGlobal(tb); }
    // SystemExtendedHandleInformation = 64: count, reserved, then 40-byte entries on x64.
    IntPtr hb = Query((b, l) => NtQuerySystemInformation(64, b, l, out r));
    var counts = new Dictionary<string, int>(); total = 0;
    try {
      long n = Marshal.ReadInt64(hb);
      IntPtr e = hb + 16;
      for (long i = 0; i < n; i++, e = e + 40) {
        long owner = Marshal.ReadInt64(e, 8);
        if (owner != pid) continue;
        int ti = Marshal.ReadInt16(e, 30);
        string name = names.ContainsKey(ti) ? names[ti] : ("type" + ti);
        counts[name] = (counts.ContainsKey(name) ? counts[name] : 0) + 1; total++;
      }
    } finally { Marshal.FreeHGlobal(hb); }
    return counts;
  }
}
'@
$total = 0
$types = [SeHandles]::ByType($ProcessId, [ref]$total)
$gpu = @()
try {
  $ded = (Get-Counter "\GPU Process Memory(pid_$($ProcessId)_*)\Dedicated Usage" -ErrorAction Stop).CounterSamples
  $sh = (Get-Counter "\GPU Process Memory(pid_$($ProcessId)_*)\Shared Usage" -ErrorAction Stop).CounterSamples
  foreach ($d in $ded) {
    $luid = if ($d.InstanceName -match 'luid_(0x[0-9a-f]+_0x[0-9a-f]+)') { $Matches[1] } else { $d.InstanceName }
    $s = $sh | Where-Object { $_.InstanceName -eq $d.InstanceName } | Select-Object -First 1
    $gpu += [pscustomobject]@{ adapter = $luid; dedicatedMB = [math]::Round($d.CookedValue / 1MB, 1); sharedMB = if ($s) { [math]::Round($s.CookedValue / 1MB, 1) } else { $null } }
  }
} catch { }
$t = @{}; foreach ($k in $types.Keys) { $t[$k] = $types[$k] }
[pscustomobject]@{ total = $total; types = $t; gpu = $gpu } | ConvertTo-Json -Compress -Depth 4
