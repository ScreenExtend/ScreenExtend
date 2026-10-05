# A 48 px borderless window that slides across the non-primary display. It never takes focus and
# never touches the cursor or the keyboard.
param([int]$Seconds = 20)
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing -TypeDefinition @'
using System; using System.Windows.Forms;
public class Mover : Form {
  protected override bool ShowWithoutActivation { get { return true; } }
  protected override CreateParams CreateParams { get { var p = base.CreateParams; p.ExStyle |= 0x08000000 | 0x80 | 0x8; return p; } }
}
'@
$screens = [System.Windows.Forms.Screen]::AllScreens
$t = $screens | Where-Object { -not $_.Primary } | Select-Object -First 1
if (-not $t) { "no secondary display; screens=$($screens.Count)"; exit 1 }
$b = $t.Bounds
$f = New-Object Mover
$f.FormBorderStyle = 'None'; $f.StartPosition = 'Manual'; $f.ShowInTaskbar = $false; $f.TopMost = $true
$f.Size = New-Object System.Drawing.Size(48, 48); $f.BackColor = [System.Drawing.Color]::OrangeRed
$f.Location = New-Object System.Drawing.Point(($b.X + 20), ($b.Y + 200))
$f.Show()
$sw = [System.Diagnostics.Stopwatch]::StartNew(); $n = 0
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
  $x = $b.X + 20 + [int](($sw.ElapsedMilliseconds * 0.6) % ($b.Width - 100))
  $f.Location = New-Object System.Drawing.Point($x, ($b.Y + 200))
  [System.Windows.Forms.Application]::DoEvents(); $n++
  Start-Sleep -Milliseconds 4
}
$f.Close()
"moved on $($t.DeviceName) $($b.Width)x$($b.Height) at $($b.X),$($b.Y): $n steps in $Seconds s"
