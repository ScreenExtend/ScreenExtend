# Plays a quiet 440 Hz tone on the default output for N seconds (so system-audio capture has a signal).
param([int]$Seconds = 20)
$rate = 48000; $n = $rate * $Seconds
$ms = New-Object System.IO.MemoryStream; $w = New-Object System.IO.BinaryWriter($ms)
$w.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $w.Write([int](36 + $n * 2)); $w.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt '))
$w.Write([int]16); $w.Write([int16]1); $w.Write([int16]1); $w.Write([int]$rate); $w.Write([int]($rate * 2)); $w.Write([int16]2); $w.Write([int16]16)
$w.Write([Text.Encoding]::ASCII.GetBytes('data')); $w.Write([int]($n * 2))
$buf = New-Object byte[] ($n * 2)
for ($i = 0; $i -lt $n; $i++) { $v = [int16](2600 * [Math]::Sin(2 * [Math]::PI * 440 * $i / $rate)); $buf[2*$i] = $v -band 0xFF; $buf[2*$i+1] = ($v -shr 8) -band 0xFF }
$w.Write($buf); $ms.Position = 0
$p = New-Object System.Media.SoundPlayer($ms); $p.PlaySync()
