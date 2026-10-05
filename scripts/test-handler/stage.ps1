# The test's on-screen content, drawn on ONE display, named by its GDI device name. Never
# takes focus, never touches the cursor or the keyboard: every window is WS_EX_NOACTIVATE |
# WS_EX_TRANSPARENT (click-through) | WS_EX_TOOLWINDOW, shown without activation.
#
#   -Stamp      a row of cells at the display's top-left encoding the host's wall-clock time in
#               milliseconds (24 bits), redrawn about every millisecond so the frame the
#               compositor picks up carries a time at most ~1-2 ms old
#   -Mover      a 48 px window sliding across the display (the stand-in for a cursor)
#   -Large      a large region repainted with shifted noise every 16 ms (sustained full motion)
#
# Prints one JSON line when the display is found ({"ready":...}) and one when done.
# Cell layout (left to right): guard W B W B, 24 data bits MSB first (white = 1), 8 check bits
# (data bytes summed, XOR 0x5A), end guard B W.
param(
  [string]$Gdi = '',
  [int]$DriverId = -1,            # the driver's monitor id; its device path carries UID<256+id>
  [string]$NewScreenNotIn = '',   # wait for a screen not in this ';'-list (a join's display) and use it
  [int]$Seconds = 20,
  [switch]$Stamp, [switch]$Mover, [switch]$Large,
  [int]$WaitSeconds = 15,
  # Mover motion: one step every MoverStepMs at MoverSpeed px/ms. With MoveSeconds > 0 the mover
  # alternates MoveSeconds of motion with HoldSeconds standing still, and prints
  # {"stop":n,"x":..,"y":..,"t":epoch-ms} (display-relative) the moment each hold begins, so a
  # reader can check where the client's picture comes to rest against where the window really is.
  [int]$MoverStepMs = 16, [double]$MoverSpeed = 0.6,
  [double]$MoveSeconds = 0, [double]$HoldSeconds = 0
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Runtime.InteropServices;
using System.Windows.Forms;

public class StageForm : Form {
  protected override bool ShowWithoutActivation { get { return true; } }
  protected override CreateParams CreateParams { get {
    var p = base.CreateParams;
    p.ExStyle |= 0x08000000 /*NOACTIVATE*/ | 0x80 /*TOOLWINDOW*/ | 0x8 /*TOPMOST*/ | 0x20 /*TRANSPARENT*/;
    return p; } }
  protected override void OnPaintBackground(PaintEventArgs e) { }
}

public static class Stage {
  [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr v);
  [DllImport("winmm.dll")] public static extern uint timeBeginPeriod(uint p);
  [DllImport("user32.dll")] static extern int GetDisplayConfigBufferSizes(uint flags, out uint nPath, out uint nMode);
  [DllImport("user32.dll")] static extern int QueryDisplayConfig(uint flags, ref uint nPath, byte[] paths, ref uint nMode, byte[] modes, IntPtr topo);
  [DllImport("user32.dll")] static extern int DisplayConfigGetDeviceInfo(byte[] packet);

  public static void DpiAware() { SetProcessDpiAwarenessContext(new IntPtr(-4)); }

  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool EnumDisplaySettingsW(string dev, int mode, byte[] dm);
  // {x, y, width, height} of a display from its current mode (DEVMODEW read as bytes), or null.
  // Read fresh every call, unlike Screen.AllScreens, which caches for the life of the process.
  public static int[] Bounds(string gdi) {
    var dm = new byte[220]; BitConverter.GetBytes((short)220).CopyTo(dm, 68);
    if (!EnumDisplaySettingsW(gdi, -1, dm)) return null;
    int w = BitConverter.ToInt32(dm, 172), h = BitConverter.ToInt32(dm, 176);
    if (w <= 0 || h <= 0) return null;
    return new int[] { BitConverter.ToInt32(dm, 76), BitConverter.ToInt32(dm, 80), w, h };
  }

  static string W(byte[] b, int off, int chars) {
    var s = System.Text.Encoding.Unicode.GetString(b, off, chars * 2);
    int z = s.IndexOf('\0'); return z >= 0 ? s.Substring(0, z) : s;
  }

  // [gdiName, friendlyName] for every active path (QueryDisplayConfig, QDC_ONLY_ACTIVE_PATHS).
  public static string[][] Paths() {
    uint np, nm;
    if (GetDisplayConfigBufferSizes(2, out np, out nm) != 0) return new string[0][];
    var paths = new byte[np * 72]; var modes = new byte[nm * 64];
    if (QueryDisplayConfig(2, ref np, paths, ref nm, modes, IntPtr.Zero) != 0) return new string[0][];
    var res = new string[np][];
    for (int i = 0; i < np; i++) {
      int o = i * 72;
      var src = new byte[84];
      BitConverter.GetBytes(1).CopyTo(src, 0); BitConverter.GetBytes(84).CopyTo(src, 4);
      Array.Copy(paths, o, src, 8, 8); Array.Copy(paths, o + 8, src, 16, 4);
      var tgt = new byte[420];
      BitConverter.GetBytes(2).CopyTo(tgt, 0); BitConverter.GetBytes(420).CopyTo(tgt, 4);
      Array.Copy(paths, o + 20, tgt, 8, 8); Array.Copy(paths, o + 28, tgt, 16, 4);
      string gdi = DisplayConfigGetDeviceInfo(src) == 0 ? W(src, 20, 32) : "";
      bool ok = DisplayConfigGetDeviceInfo(tgt) == 0;
      string fr = ok ? W(tgt, 36, 64) : "";
      string dp = ok ? W(tgt, 164, 128) : "";
      res[i] = new string[] { gdi, fr, dp };
    }
    return res;
  }

  // Drawn into an off-screen bitmap and copied to the window in one blit, so a capture never
  // catches half a stamp (the check bits catch it if one does).
  static Bitmap buf; static Graphics bg;
  public static void ResetStamp() { buf = null; bg = null; }
  public static void BlitStamp(Graphics win, long ms, int cell, int h) {
    if (buf == null) { buf = new Bitmap(38 * cell, h, System.Drawing.Imaging.PixelFormat.Format32bppPArgb); bg = Graphics.FromImage(buf); }
    DrawStamp(bg, ms, cell, h);
    win.CompositingMode = System.Drawing.Drawing2D.CompositingMode.SourceCopy;
    win.DrawImageUnscaled(buf, 0, 0);
  }
  public static void DrawStamp(Graphics g, long ms, int cell, int h) {
    uint v = (uint)(ms & 0xFFFFFF);
    int sum = (int)(((v >> 16) & 0xFF) + ((v >> 8) & 0xFF) + (v & 0xFF)) & 0xFF;
    int chk = sum ^ 0x5A;
    int x = 0;
    bool[] bits = new bool[38];
    bits[0] = true; bits[1] = false; bits[2] = true; bits[3] = false;
    for (int i = 0; i < 24; i++) bits[4 + i] = ((v >> (23 - i)) & 1) == 1;
    for (int i = 0; i < 8; i++) bits[28 + i] = ((chk >> (7 - i)) & 1) == 1;
    bits[36] = false; bits[37] = true;
    for (int i = 0; i < 38; i++) { g.FillRectangle(bits[i] ? Brushes.White : Brushes.Black, x, 0, cell, h); x += cell; }
  }
}
'@
[Stage]::DpiAware()
[Stage]::timeBeginPeriod(1) | Out-Null

# The handler names the display (the host's own report of its \\.\DISPLAYn, or the one screen
# that appeared with the join): the bundled driver gives every monitor the same EDID name, so
# the monitor name cannot tell two virtual displays apart.
$deadline = (Get-Date).AddSeconds($WaitSeconds); $scr = $null
$known = @($NewScreenNotIn -split ';' | Where-Object { $_ })
$via = if ($Gdi) { 'given name' } else { 'the one new screen' }
$bounds = $null
while (-not $bounds -and (Get-Date) -lt $deadline) {
  $paths = [Stage]::Paths()
  if ($DriverId -ge 0) {
    # A \\.\DISPLAYn name is a slot Windows reassigns as displays come and go; the monitor's
    # device path is stable, and the bundled driver's carries UID<256 + id>.
    $want = "UID$(256 + $DriverId)#"
    $hit = @($paths | Where-Object { $_[2] -and $_[2].Contains($want) })
    if ($hit.Count -eq 1) { $Gdi = $hit[0][0]; $via = "driver id $DriverId -> $want" } else { $via = "driver id $DriverId matched $($hit.Count) paths; fell back to the given name" }
  }
  if (-not $Gdi -or $via -eq 'the one new screen') {
    $new = @($paths | Where-Object { $_[0] -and $known -notcontains $_[0] })
    if ($new.Count -eq 1) { $Gdi = $new[0][0] } elseif ($new.Count -gt 1) { "{""error"":""$($new.Count) new screens appeared""}"; exit 3 }
  }
  if ($Gdi) { $bounds = [Stage]::Bounds($Gdi) }
  if (-not $bounds) { Start-Sleep -Milliseconds 40 }
}
if (-not $bounds) { $all = ([Stage]::Paths() | ForEach-Object { $_[0] }) -join ';'; "{""error"":""display $($Gdi -replace '\\','/') not attached"",""screens"":""$($all -replace '\\','/')""}"; exit 2 }
$gdi = $Gdi -replace '\\', '\\'
$b = New-Object System.Drawing.Rectangle($bounds[0], $bounds[1], $bounds[2], $bounds[3])
$cell = [Math]::Max(8, [Math]::Min(64, [int][Math]::Floor($b.Width / 40)))
$cellH = $cell

$forms = @()
if ($Stamp) {
  $sf = New-Object StageForm
  $sf.FormBorderStyle = 'None'; $sf.StartPosition = 'Manual'; $sf.ShowInTaskbar = $false; $sf.TopMost = $true
  $sf.Bounds = New-Object System.Drawing.Rectangle($b.X, $b.Y, (38 * $cell), $cellH)
  $sf.Show(); $forms += $sf
  $sg = $sf.CreateGraphics()
}
if ($Mover) {
  $mf = New-Object StageForm
  $mf.FormBorderStyle = 'None'; $mf.StartPosition = 'Manual'; $mf.ShowInTaskbar = $false; $mf.TopMost = $true
  $mf.BackColor = [System.Drawing.Color]::OrangeRed
  $mf.Bounds = New-Object System.Drawing.Rectangle(($b.X + 20), ($b.Y + [int]($b.Height * 0.4)), 48, 48)
  $mf.Show(); $forms += $mf
  $mg = $mf.CreateGraphics()
}
if ($Large) {
  $lf = New-Object StageForm
  $lf.FormBorderStyle = 'None'; $lf.StartPosition = 'Manual'; $lf.ShowInTaskbar = $false; $lf.TopMost = $true
  $lw = [int]($b.Width * 0.8); $lh = [int](($b.Height - 3 * $cellH) * 0.8)
  $lf.Bounds = New-Object System.Drawing.Rectangle(($b.X + [int]($b.Width * 0.1)), ($b.Y + 3 * $cellH), $lw, $lh)
  $lf.Show(); $forms += $lf
  $lg = $lf.CreateGraphics()
  $noise = New-Object System.Drawing.Bitmap(($lw + 256), ($lh + 256), [System.Drawing.Imaging.PixelFormat]::Format32bppPArgb)
  $lg.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
  $lg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
  $rnd = New-Object System.Random 7
  $ng = [System.Drawing.Graphics]::FromImage($noise)
  for ($y = 0; $y -lt $noise.Height; $y += 16) { for ($x = 0; $x -lt $noise.Width; $x += 16) {
    $c = [System.Drawing.Color]::FromArgb($rnd.Next(256), $rnd.Next(256), $rnd.Next(256))
    $br = New-Object System.Drawing.SolidBrush $c; $ng.FillRectangle($br, $x, $y, 16, 16); $br.Dispose() } }
  $ng.Dispose()
}
function Ready-Line { "{""ready"":true,""gdi"":""$gdi"",""x"":$($b.X),""y"":$($b.Y),""width"":$($b.Width),""height"":$($b.Height),""cell"":$cell,""cellH"":$cellH,""cells"":38,""via"":""$via""}"; [Console]::Out.Flush() }
Ready-Line
[Console]::Out.Flush()

$epoch = [DateTime]::new(1970, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
$sw = [System.Diagnostics.Stopwatch]::StartNew(); $draws = 0; $lastSlow = -100
$lastCheck = 0
$holding = $false; $phaseStart = 0; $stops = 0; $moveClock = 0
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
  if ($Stamp) {
    $ms = [long]([DateTime]::UtcNow - $epoch).TotalMilliseconds
    [Stage]::BlitStamp($sg, $ms, $cell, $cellH); $draws++
  }
  $now = $sw.ElapsedMilliseconds
  # A display just attached is often still changing mode (the host sets the client's resolution
  # right after the attach). Follow it, and say so: the reader's cell geometry must match.
  if ($now - $lastCheck -ge 200) {
    $lastCheck = $now
    $nb = [Stage]::Bounds($Gdi)
    if ($nb -and ($nb[0] -ne $b.X -or $nb[1] -ne $b.Y -or $nb[2] -ne $b.Width -or $nb[3] -ne $b.Height)) {
      $b = New-Object System.Drawing.Rectangle($nb[0], $nb[1], $nb[2], $nb[3])
      $cell = [Math]::Max(8, [Math]::Min(64, [int][Math]::Floor($b.Width / 40))); $cellH = $cell
      if ($Stamp) { $sf.Bounds = New-Object System.Drawing.Rectangle($b.X, $b.Y, (38 * $cell), $cellH); $sg.Dispose(); $sg = $sf.CreateGraphics(); [Stage]::ResetStamp() }
      if ($Mover) { $mf.Location = New-Object System.Drawing.Point(($b.X + 20), ($b.Y + [int]($b.Height * 0.4))) }
      Ready-Line
    }
  }
  $step = if ($Mover) { $MoverStepMs } else { 16 }
  if ($now - $lastSlow -ge $step) {
    $dt = $now - $lastSlow; $lastSlow = $now
    if ($Mover) {
      if ($MoveSeconds -gt 0) {
        $phaseLen = if ($holding) { $HoldSeconds * 1000 } else { $MoveSeconds * 1000 }
        if ($now - $phaseStart -ge $phaseLen) {
          $holding = -not $holding; $phaseStart = $now
          if ($holding) {
            $stops++
            $ux = [long]([DateTime]::UtcNow - $epoch).TotalMilliseconds
            "{""stop"":$stops,""x"":$($mf.Location.X - $b.X),""y"":$($mf.Location.Y - $b.Y),""w"":48,""t"":$ux}"; [Console]::Out.Flush()
          }
        }
        if (-not $holding) { $moveClock += [Math]::Min($dt, 4 * $step) }
        $x = $b.X + 20 + [int](($moveClock * $MoverSpeed) % ($b.Width - 100))
      } else {
        $x = $b.X + 20 + [int](($now * $MoverSpeed) % ($b.Width - 100))
      }
      if ($mf.Location.X -ne $x) { $mf.Location = New-Object System.Drawing.Point($x, ($b.Y + [int]($b.Height * 0.4))) }
    }
    if ($Large) { $off = [int](($now / 16) % 256); $lg.DrawImageUnscaled($noise, -$off, -((($now / 7) % 256) -as [int])) }
    [System.Windows.Forms.Application]::DoEvents()
  }
  if (-not $Stamp -and -not $Mover) { Start-Sleep -Milliseconds 4 } else { [System.Threading.Thread]::Sleep(1) }
}
foreach ($f in $forms) { $f.Close() }
"{""done"":true,""stampDraws"":$draws,""seconds"":$Seconds}"
