# Moves the REAL cursor in a sweep across one display, then puts it back. Used only by requests
# that set allow_real_input. Never clicks, never types. Aborts when the cursor is not where this
# script last put it (someone else moved it) or when it would leave the named display.
#
#   -Gdi \\.\DISPLAYn   the test's virtual display
#   -Seconds N          length of the sweep
#   -StepMs N           one move every N ms (default 4)
#   -Get / -Put x,y     read or put back the cursor position
# Prints one JSON line when done.
param([string]$Gdi = '', [double]$Seconds = 10, [int]$StepMs = 4, [switch]$Get, [string]$Put = '')
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class SeCur {
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
}
'@
if ($Get) { $p = New-Object SeCur+POINT; [void][SeCur]::GetCursorPos([ref]$p); "{`"x`":$($p.X),`"y`":$($p.Y)}"; exit 0 }
if ($Put) { $xy = $Put.Split(','); [void][SeCur]::SetCursorPos([int]$xy[0], [int]$xy[1]); '{"restored":true}'; exit 0 }
$screen = [System.Windows.Forms.Screen]::AllScreens | Where-Object { $_.DeviceName -eq $Gdi }
if (-not $screen) { '{"error":"display not found"}'; exit 2 }
if ($screen.Primary) { '{"error":"refusing to sweep the primary display"}'; exit 2 }
$b = $screen.Bounds
$saved = New-Object SeCur+POINT; [void][SeCur]::GetCursorPos([ref]$saved)
$steps = 0; $aborted = $null; $last = $null
$sw = [System.Diagnostics.Stopwatch]::StartNew()
try {
  $y0 = $b.Top + [int]($b.Height * 0.55); $amp = [int]($b.Height * 0.2)
  while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
    if ($last) {
      $now = New-Object SeCur+POINT; [void][SeCur]::GetCursorPos([ref]$now)
      if ([math]::Abs($now.X - $last.X) -gt 2 -or [math]::Abs($now.Y - $last.Y) -gt 2) { $aborted = "cursor moved by something else to $($now.X),$($now.Y)"; break }
    }
    $t = $sw.Elapsed.TotalMilliseconds
    $x = $b.Left + 40 + [int](($b.Width - 80) * (0.5 + 0.5 * [math]::Sin($t / 900.0)))
    $y = $y0 + [int]($amp * [math]::Sin($t / 370.0))
    if (-not $b.Contains($x, $y)) { $aborted = 'sweep would leave the display'; break }
    [void][SeCur]::SetCursorPos($x, $y); $last = @{ X = $x; Y = $y }; $steps++
    $next = $t + $StepMs; while ($sw.Elapsed.TotalMilliseconds -lt $next) { [System.Threading.Thread]::Sleep(0) }
  }
} finally {
  [void][SeCur]::SetCursorPos($saved.X, $saved.Y)
}
@{ steps = $steps; seconds = [math]::Round($sw.Elapsed.TotalSeconds, 2); aborted = $aborted; restoredTo = "$($saved.X),$($saved.Y)"; display = "$($b.Left),$($b.Top) $($b.Width)x$($b.Height)" } | ConvertTo-Json -Compress
