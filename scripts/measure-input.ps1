<#
.SYNOPSIS
  Measures the client -> host input leg, and what it costs to *see* the result.

.DESCRIPTION
  `measure-stream.ps1` measures the host -> client video leg with the host's own pointer. This
  measures the other direction, plus the one number that joins them: how long after a *client*
  pointer move a frame carrying the new cursor position reaches the client.

  Four phases, each reported separately because the fixes are different:

  1. hold      — inside the page: DOM event -> `RTCDataChannel.send`. Catches a send-side
                 coalescer holding a frame for a timer, a task or a byte threshold. Needs no host
                 instrumentation: it hooks `RTCDataChannel.prototype.send` and a capture-phase
                 pointer listener on `window`, which runs ahead of the page's own.
  2. forward   — client send -> host `SendInput`, paired in order against the host's
                 `input-trace` lines (`SE_INPUT_TRACE=1`). Both stamps are wall clock on one
                 machine, so they are directly comparable.
  3. frames    — the decisive one for "the client pointer feels laggy". Painted fps over three
                 windows on an otherwise static desktop: nothing moving, the *client's* pointer
                 sweeping, the *host's* pointer sweeping. A host that only produces a frame when
                 the desktop changes shows up here as a client sweep that paints nothing.
  4. keys      — keystrokes into a focused field on the virtual display, same pairing as (2).

  The virtual display is left genuinely static for phase 3: no motion window, no animation. That
  is the scene the report is about.

.EXAMPLE
  scripts/measure-input.ps1 -Binary C:\...\ScreenExtend.exe -Label head-before
  scripts/measure-input.ps1 -Binary C:\...\ScreenExtend.exe -Label head-before -Load
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$Label,
    [int]$HttpPort = 8080,
    [int]$HttpsPort = 8443,
    # Seconds per frame-production window in phase 3.
    [int]$Window = 8,
    [int]$CdpPort = 0,
    [string]$OutDir = "$env:TEMP\se-measure-input",
    # Busy-loop on ProcessorCount-2 cores plus a GPU-ish canvas loop for the whole run.
    [switch]$Load
)

$ErrorActionPreference = 'Stop'
if ($CdpPort -eq 0) { $CdpPort = Get-Random -Minimum 9400 -Maximum 9899 }
if (-not (Test-Path $Binary)) { throw "no binary at $Binary" }
$null = New-Item -ItemType Directory -Force -Path $OutDir
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$runDir = Join-Path $OutDir "$Label-$stamp"
$null = New-Item -ItemType Directory -Force -Path $runDir
Start-Transcript -Path (Join-Path $runDir 'harness.log') -Force | Out-Null
function Step($m) { Write-Host ("[{0:HH:mm:ss}] {1}" -f (Get-Date), $m) }

# A JSON array from the page as a flat PowerShell array.
#
# PowerShell 5.1's `ConvertFrom-Json` emits a JSON array as a single `Object[]` rather than
# unrolling it into the pipeline, so the obvious `@($json | ConvertFrom-Json)` produces a
# one-element array *containing* the array — and `$a[0]` is then a whole column of values, which
# fails on the first arithmetic with "[System.Object[]] does not contain a method named
# 'op_Subtraction'". Piping through `ForEach-Object` is what unrolls it.
function JsonArray($json) {
    if (-not $json) { return @() }
    $v = $json | ConvertFrom-Json
    if ($null -eq $v) { return @() }
    return @($v | ForEach-Object { $_ })
}

# Pair each client send with the injection it caused.
#
# Index pairing (`trace[k] - send[k]`) is wrong the moment either side has an extra or a missing
# row: one unmatched injection shifts every later pair by a whole inter-event gap, and the run
# reports a 250 ms "latency" that is really one dropped frame. Greedy forward matching instead —
# each send takes the earliest injection at or after it that no earlier send has taken — so a drop
# costs exactly one unmatched send and nothing downstream of it.
#
# A stray injection *before* the first send (the harness parks the host pointer) matches nothing
# and is counted, not silently absorbed.
#
# `$Tol` absorbs the skew between the two clocks. The page's stamp is `Date.now() - performance.now()`
# plus `performance.now()`, the host's is `SystemTime::now()`, and the two disagree by a few
# milliseconds — so a genuine injection can carry a stamp slightly *earlier* than the send that
# caused it. Without the tolerance that injection is skipped and the send matches the *next* one,
# which at a 250 ms cadence reports a 250 ms latency that never happened.
#
# `$MaxMs` keeps a loss local. Without a ceiling, one missing injection makes that send adopt the
# *next* send's injection and every later pair inherits the shift, so a single dropped frame at a
# 250 ms cadence turns the whole run into a 250 ms "latency". An injection that far behind its send
# is not that send's injection; the send is unmatched and the cursor stays put.
function Pair-Forward($sends, $trace, $Tol = 25, $MaxMs = 200) {
    $deltas = @()
    $unmatched = 0
    $j = 0
    foreach ($s in $sends) {
        while ($j -lt $trace.Count -and $trace[$j].wall -lt ($s - $Tol)) { $j++ }
        if ($j -ge $trace.Count -or ($trace[$j].wall - $s) -gt $MaxMs) { $unmatched++; continue }
        $deltas += ($trace[$j].wall - $s)
        $j++
    }
    return [pscustomobject]@{
        deltas      = $deltas
        matched     = $deltas.Count
        unmatched   = $unmatched
        extraBefore = ($trace | Where-Object { $sends.Count -gt 0 -and $_.wall -lt $sends[0] }).Count
    }
}

# Percentiles over a PowerShell array, reported to 0.1 ms. Returned as a hashtable so the caller
# can drop it straight into the JSON.
function Pct($values, $scale = 1.0) {
    if (-not $values -or $values.Count -eq 0) { return $null }
    $s = @($values | Sort-Object)
    function at($f) { $s[[Math]::Min($s.Count - 1, [int][Math]::Floor($s.Count * $f))] }
    return [ordered]@{
        n    = $s.Count
        p50  = [Math]::Round((at 0.50) * $scale, 2)
        p95  = [Math]::Round((at 0.95) * $scale, 2)
        p99  = [Math]::Round((at 0.99) * $scale, 2)
        max  = [Math]::Round(($s[$s.Count - 1]) * $scale, 2)
        mean = [Math]::Round((($s | Measure-Object -Average).Average) * $scale, 2)
    }
}

# --------------------------------------------------------------------------------------------
# SendInput. The `INPUT` struct is 40 bytes on x64; a 48-byte one makes every call return 0 with
# ERROR_INVALID_PARAMETER and the pointer never moves, which is a whole measurement of the wrong
# thing that looks exactly like a real one. `SelfTest` refuses to let that happen quietly.
# --------------------------------------------------------------------------------------------
if (-not ('SEIn' -as [type])) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class SEIn {
    [StructLayout(LayoutKind.Sequential)]
    struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr extra; }
    [StructLayout(LayoutKind.Sequential)]
    struct INPUT { public uint type; public MOUSEINPUT mi; }
    [DllImport("user32.dll", SetLastError = true)] static extern uint SendInput(uint n, INPUT[] p, int cb);
    [DllImport("user32.dll")] static extern int GetSystemMetrics(int i);
    public static void MoveTo(int x, int y) {
        int vx = GetSystemMetrics(76), vy = GetSystemMetrics(77);
        int vw = GetSystemMetrics(78), vh = GetSystemMetrics(79);
        if (vw <= 1) vw = 1; if (vh <= 1) vh = 1;
        INPUT[] inp = new INPUT[1];
        inp[0].type = 0;
        inp[0].mi.dx = (int)(((double)(x - vx) * 65535.0) / (vw - 1));
        inp[0].mi.dy = (int)(((double)(y - vy) * 65535.0) / (vh - 1));
        inp[0].mi.dwFlags = 0x0001 | 0x8000 | 0x4000;
        SendInput(1, inp, Marshal.SizeOf(typeof(INPUT)));
    }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int x, y; }
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    public static string SelfTest(int x, int y) {
        POINT before; GetCursorPos(out before);
        MoveTo(x, y);
        System.Threading.Thread.Sleep(120);
        POINT after; GetCursorPos(out after);
        MoveTo(before.x, before.y);
        int dx = after.x - x, dy = after.y - y;
        if (dx < -2 || dx > 2 || dy < -2 || dy > 2) {
            return "asked for " + x + "," + y + " and the pointer is at " + after.x + "," + after.y
                 + " (INPUT is " + Marshal.SizeOf(typeof(INPUT)) + " bytes)";
        }
        return null;
    }
    [StructLayout(LayoutKind.Sequential)] struct RECT { public int left, top, right, bottom; }
    [StructLayout(LayoutKind.Sequential)] struct MONITORINFO { public int cbSize; public RECT rc; public RECT work; public uint flags; }
    delegate bool MonEnum(IntPtr h, IntPtr dc, ref RECT r, IntPtr data);
    [DllImport("user32.dll")] static extern bool EnumDisplayMonitors(IntPtr dc, IntPtr clip, MonEnum cb, IntPtr data);
    [DllImport("user32.dll")] static extern bool GetMonitorInfo(IntPtr h, ref MONITORINFO mi);
    public static string[] Monitors() {
        var list = new System.Collections.Generic.List<string>();
        MonEnum cb = delegate (IntPtr h, IntPtr dc, ref RECT r, IntPtr data) {
            MONITORINFO mi = new MONITORINFO();
            mi.cbSize = Marshal.SizeOf(typeof(MONITORINFO));
            if (GetMonitorInfo(h, ref mi)) {
                list.Add(string.Format("{0},{1},{2},{3},{4}", mi.rc.left, mi.rc.top,
                    mi.rc.right - mi.rc.left, mi.rc.bottom - mi.rc.top, (mi.flags & 1) != 0 ? 1 : 0));
            }
            return true;
        };
        EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, cb, IntPtr.Zero);
        return list.ToArray();
    }
}
'@
}

# --------------------------------------------------------------------------------------------
# CDP over a raw WebSocket, same shape as measure-stream.ps1.
# --------------------------------------------------------------------------------------------
$script:cdpSocket = $null
$script:cdpId = 0

function Connect-Cdp {
    param([string]$WsUrl)
    $ws = New-Object System.Net.WebSockets.ClientWebSocket
    $ws.Options.KeepAliveInterval = [TimeSpan]::FromSeconds(30)
    $ws.ConnectAsync([Uri]$WsUrl, [Threading.CancellationToken]::None).Wait(10000) | Out-Null
    if ($ws.State -ne 'Open') { throw "CDP websocket did not open (state $($ws.State))" }
    $script:cdpSocket = $ws
}

function Invoke-Cdp {
    param([string]$Method, $Params = @{}, [int]$TimeoutMs = 20000, [string]$SessionId)
    $script:cdpId++
    $myId = $script:cdpId
    $msg = @{ id = $myId; method = $Method; params = $Params }
    if ($SessionId) { $msg.sessionId = $SessionId }
    $bytes = [Text.Encoding]::UTF8.GetBytes(($msg | ConvertTo-Json -Depth 10 -Compress))
    $seg = New-Object 'System.ArraySegment[byte]' -ArgumentList @(, $bytes)
    $script:cdpSocket.SendAsync($seg, 'Text', $true, [Threading.CancellationToken]::None).Wait() | Out-Null
    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $sb = New-Object Text.StringBuilder
        do {
            $buf = New-Object byte[] 262144
            $rseg = New-Object 'System.ArraySegment[byte]' -ArgumentList @(, $buf)
            $task = $script:cdpSocket.ReceiveAsync($rseg, [Threading.CancellationToken]::None)
            if (-not $task.Wait($TimeoutMs)) { throw "CDP receive timed out on $Method" }
            $r = $task.Result
            [void]$sb.Append([Text.Encoding]::UTF8.GetString($buf, 0, $r.Count))
        } while (-not $r.EndOfMessage)
        $m = $sb.ToString() | ConvertFrom-Json
        if ($m.PSObject.Properties.Name -contains 'id' -and $m.id -eq $myId) {
            if ($m.PSObject.Properties.Name -contains 'error') { throw "CDP $Method failed: $($m.error.message)" }
            return $m.result
        }
    }
    throw "no CDP reply to $Method"
}

function Invoke-Js {
    param([string]$Expression, [int]$TimeoutMs = 20000, [switch]$UserGesture, [string]$SessionId)
    $r = Invoke-Cdp 'Runtime.evaluate' @{
        expression = $Expression; returnByValue = $true; awaitPromise = $true
        userGesture = [bool]$UserGesture
    } $TimeoutMs $SessionId
    if ($r.exceptionDetails) { throw "page threw: $($r.exceptionDetails.text)" }
    return $r.result.value
}

# --------------------------------------------------------------------------------------------
# Host
#
# Only one ScreenExtend can hold the single-instance lock, and on this machine more than one agent
# runs one. So the lock is taken by *identifying* the holder rather than by killing everything
# named ScreenExtend: a host belonging to another agent's target directory is waited for, and the
# owner's GUI is stopped and relaunched afterwards because the owner uses it between runs.
# --------------------------------------------------------------------------------------------
$OwnerGui = 'C:\Users\sarve\.cargo\shared_target\ScreenExtend\release\ScreenExtend.exe'
# Target directories this run may stop a host in. Anything else is someone else's.
$MineRoots = @(
    'C:\Users\sarve\.cargo\shared_target\ScreenExtend-w2\',
    'C:\Users\sarve\.cargo\shared_target\ScreenExtend-base\'
)

function Se-Hosts {
    Get-CimInstance Win32_Process -Filter "Name='ScreenExtend.exe'" -ErrorAction SilentlyContinue |
        ForEach-Object { [pscustomobject]@{ Pid = $_.ProcessId; Path = $_.ExecutablePath; Cmd = $_.CommandLine } }
}

function Claim-HostLock {
    for ($i = 0; $i -lt 120; $i++) {
        $others = @()
        foreach ($h in Se-Hosts) {
            $p = "$($h.Path)"
            if ($p -eq $OwnerGui) {
                Step "stopping the owner's GUI (pid $($h.Pid)); it is relaunched when the run ends"
                Stop-Process -Id $h.Pid -Force -EA SilentlyContinue
                continue
            }
            if ($MineRoots | Where-Object { $p -like "$_*" }) {
                Stop-Process -Id $h.Pid -Force -EA SilentlyContinue
                continue
            }
            $others += $h
        }
        if ($others.Count -eq 0) { Start-Sleep -Milliseconds 900; return }
        Step ("waiting for another agent's host to finish: " + (($others | ForEach-Object { "pid $($_.Pid) $($_.Path)" }) -join '; '))
        Start-Sleep -Seconds 30
    }
    throw 'another agent has held the single-instance lock for an hour; not forcing it'
}

function Start-OwnerGui {
    if (-not (Test-Path $OwnerGui)) { Write-Warning "the owner's GUI is not built at $OwnerGui"; return }
    if (Se-Hosts | Where-Object { "$($_.Path)" -eq $OwnerGui }) { return }
    $g = Start-Process -FilePath $OwnerGui -PassThru
    # Launching it is not the same as it working: a `config.json` the bootstrap cannot replay stops
    # it at a Setup Error dialog, and "relaunched, pid N" would be a true statement about a host
    # that never came up. Listening on its ports is the cheapest proof it got past setup.
    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Milliseconds 500
        if ($g.HasExited) { Write-Warning "the owner's GUI exited during startup"; return }
        $listening = (Get-NetTCPConnection -State Listen -EA SilentlyContinue |
                Where-Object { $_.LocalPort -in 8080, 8443 }).Count
        if ($listening -gt 0) { Step "relaunched the owner's GUI, pid $($g.Id) (serving)"; return }
    }
    Write-Warning "the owner's GUI (pid $($g.Id)) is running but never served 8080/8443 -- check it reached the dashboard"
}

# Put the owner's `config.json` back, and never put back something the bootstrap cannot read.
#
# Idempotent, and called from the trap as well as the normal exit, because the first version of
# this harness cleared `devices` and then threw in phase 1 -- which left the file cleared and lost
# a saved device row. `devices` and `knownDevices` must both come back as JSON *arrays*: an object
# there ("devices": {"0": {...}}, the shape a dotted `config set` key produces) makes the
# bootstrap's replay loop throw "object is not iterable" and the GUI stops at a Setup Error. If the
# backup itself is malformed, the offending key is written as an empty array rather than restored,
# because handing back a file that cannot be read is worse than handing back one that is empty.
function Restore-Config {
    if (-not (Test-Path $storeBackup)) { return }
    try {
        $j = Get-Content -Raw $storeBackup | ConvertFrom-Json
        foreach ($k in 'devices', 'knownDevices') {
            $v = $j.$k
            if ($null -ne $v -and $v -isnot [System.Object[]]) {
                Write-Warning "the saved config's '$k' is a $($v.GetType().Name), not an array; writing [] instead of restoring it"
                $j.$k = @()
            }
        }
        # No BOM: tauri-plugin-store cannot parse one, and a store it cannot parse is silently
        # replaced by `defaultConfig` -- which throws away every device, every known device, the
        # ports, TURN and STUN on the next launch.
        [IO.File]::WriteAllText($storeFile, ($j | ConvertTo-Json -Depth 12), (New-Object Text.UTF8Encoding $false))
        $back = Get-Content -Raw $storeFile | ConvertFrom-Json
        Step ("config restored: devices={0} knownDevices={1}" -f @($back.devices).Count, @($back.knownDevices).Count)
    } catch {
        Write-Warning "could not restore the saved config from ${storeBackup}: $_"
    }
}

Claim-HostLock

# A saved device row pins geometry and refresh rate for every later join from the same address, so
# a run against one measures the row rather than the build. Cleared for the run and put back after.
# No BOM: tauri-plugin-store cannot parse one and silently falls back to defaultConfig, which
# throws away the whole saved configuration on the next launch.
$storeFile = Join-Path $env:APPDATA 'app.screenextend.desktop\config.json'
$storeBackup = Join-Path $runDir 'config.json.before'
if (Test-Path $storeFile) {
    Copy-Item $storeFile $storeBackup -Force
    try {
        $store = Get-Content -Raw $storeFile | ConvertFrom-Json
        if ($null -ne $store.devices) {
            $store.devices = @()
            [IO.File]::WriteAllText($storeFile, ($store | ConvertTo-Json -Depth 12),
                (New-Object Text.UTF8Encoding $false))
        }
    } catch { Write-Warning "could not clear the saved device row: $_" }
}

# From here the owner's config is modified, so every exit path has to put it back. `trap` fires on
# any terminating error, which is what the first version of this harness did not have: a throw in
# phase 1 left `devices` cleared and a saved device row was lost.
trap {
    Write-Warning "run failed: $_"
    Restore-Config
    Start-OwnerGui
    break
}

$alphabet = '23456789ABCDEFGHJKLMNPQRSTUVWXYZ'.ToCharArray()
$sessionId = -join (1..12 | ForEach-Object { $alphabet[(Get-Random -Maximum $alphabet.Length)] })
$otp = '{0:D6}' -f (Get-Random -Minimum 100000 -Maximum 999999)
$hostLog = Join-Path $runDir 'host.log'
$hostErr = Join-Path $runDir 'host.err.log'

$env:SE_INPUT_TRACE = '1'
$hostProc = Start-Process -FilePath $Binary -PassThru -WindowStyle Minimized `
    -ArgumentList @('serve', '-v', '--no-cloud', '--no-qr',
    '--http-port', "$HttpPort", '--session-id', $sessionId, '--otp', $otp) `
    -RedirectStandardOutput $hostLog -RedirectStandardError $hostErr

$serveHost = $null
for ($i = 0; $i -lt 120; $i++) {
    Start-Sleep -Milliseconds 500
    if (-not (Test-Path $hostLog)) { continue }
    $m = Select-String -Path $hostLog -Pattern 'http://([0-9.]+):(\d+)/\?id=' | Select-Object -First 1
    if ($m) { $serveHost = $m.Matches[0].Groups[1].Value; break }
}
if (-not $serveHost) { throw "host never printed a join URL (see $hostLog)" }
$base = "http://${serveHost}:$HttpPort"
$ready = $false
for ($i = 0; $i -lt 60; $i++) {
    if ((& curl.exe -s --max-time 3 "$base/health" 2>$null) -eq 'ok') { $ready = $true; break }
    Start-Sleep -Milliseconds 500
}
if (-not $ready) { throw "host never served /health at $base (see $hostLog)" }
Step "host up: $base session=$sessionId"

# --------------------------------------------------------------------------------------------
# Load, if asked. N-2 cores busy so the measurement is not competing with itself for every core.
# --------------------------------------------------------------------------------------------
$loadJobs = @()
if ($Load) {
    $n = [Math]::Max(1, [Environment]::ProcessorCount - 2)
    for ($i = 0; $i -lt $n; $i++) {
        $loadJobs += Start-Job -ScriptBlock {
            $sw = [Diagnostics.Stopwatch]::StartNew(); $x = 0.0
            while ($sw.Elapsed.TotalSeconds -lt 600) { for ($k = 0; $k -lt 200000; $k++) { $x += [Math]::Sqrt($k) } }
        }
    }
    Step "CPU load: $n busy cores"
}

# --------------------------------------------------------------------------------------------
# Client
# --------------------------------------------------------------------------------------------
$chrome = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
if (-not (Test-Path $chrome)) { $chrome = 'C:\Program Files (x86)\Google\Chrome\Application\chrome.exe' }
$profileDir = Join-Path $runDir 'chrome-profile'
$joinUrl = "https://${serveHost}:$HttpsPort/?id=$sessionId&otp=$otp&name=measureinput&fs=0"

Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -like '*se-measure*' -or $_.CommandLine -like "*--remote-debugging-port=$CdpPort*" } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
for ($i = 0; $i -lt 20; $i++) {
    if (-not (& curl.exe -s --max-time 1 "http://127.0.0.1:$CdpPort/json" 2>$null)) { break }
    Start-Sleep -Milliseconds 500
}

$chromeProc = Start-Process -FilePath $chrome -PassThru -ArgumentList @(
    "--remote-debugging-port=$CdpPort", "--user-data-dir=$profileDir",
    '--autoplay-policy=no-user-gesture-required', '--no-first-run', '--no-default-browser-check',
    '--disable-features=Translate,MediaRouter',
    '--ignore-certificate-errors', '--allow-insecure-localhost',
    '--new-window', '--window-position=0,0', '--window-size=1280,800', 'about:blank')

$target = $null
for ($i = 0; $i -lt 60; $i++) {
    Start-Sleep -Milliseconds 500
    $raw = & curl.exe -s --max-time 3 "http://127.0.0.1:$CdpPort/json" 2>$null
    if (-not $raw) { continue }
    try { $list = ($raw -join '') | ConvertFrom-Json } catch { continue }
    $target = $list | Where-Object { $_.type -eq 'page' -and $_.url -like 'about:blank*' } | Select-Object -First 1
    if ($target) { break }
}
if (-not $target) { throw 'Chrome never exposed a page over CDP' }
Connect-Cdp $target.webSocketDebuggerUrl
Invoke-Cdp 'Page.enable' @{} | Out-Null
try {
    $origin = ([Uri]$joinUrl).GetLeftPart([UriPartial]::Authority)
    Invoke-Cdp 'Browser.grantPermissions' @{ origin = $origin; permissions = @('audioCapture') } | Out-Null
} catch { }

Invoke-Cdp 'Page.navigate' @{ url = $joinUrl } | Out-Null
$loaded = $false
for ($i = 0; $i -lt 40; $i++) {
    Start-Sleep -Milliseconds 500
    try { if ((Invoke-Js "document.readyState === 'complete' && !!document.getElementById('connect')") -eq $true) { $loaded = $true; break } } catch { }
}
if (-not $loaded) { throw 'the join page never finished loading' }

Start-Sleep -Milliseconds 1200
for ($i = 0; $i -lt 6; $i++) {
    $state = Invoke-Js @'
(function () {
  var ids = ['sameDeviceDismiss', 'dismiss', 'compatContinue', 'permSkip'];
  var clicked = [];
  for (var i = 0; i < ids.length; i++) {
    var b = document.getElementById(ids[i]);
    if (b && b.offsetParent !== null) { b.click(); clicked.push(ids[i]); }
  }
  var c = document.getElementById('connect');
  return JSON.stringify({ clicked: clicked, disabled: !!(c && c.disabled) });
})()
'@ | ConvertFrom-Json
    if (-not $state.disabled -and $state.clicked.Count -eq 0) { break }
    Start-Sleep -Milliseconds 700
}
Invoke-Js "document.getElementById('connect').click(), true" -UserGesture | Out-Null

$joined = $false
for ($i = 0; $i -lt 160; $i++) {
    Start-Sleep -Milliseconds 500
    try {
        Invoke-Js "(function(){var b=document.getElementById('permSkip');if(b&&b.offsetParent!==null){b.click();return 1}return 0})()" | Out-Null
        $s = Invoke-Js 'window.SEVideo ? JSON.stringify(SEVideo.getStats()) : ""'
        if ($s) { $st = $s | ConvertFrom-Json; if ($st.framesPainted -gt 0) { $joined = $true; break } }
    } catch { }
}
if (-not $joined) { throw "client never painted a frame (host log: $hostLog)" }
Step 'client joined and painting'

$virtual = $null
for ($i = 0; $i -lt 20; $i++) {
    $virtual = [SEIn]::Monitors() | Where-Object { $_.Split(',')[4] -eq '0' } | Select-Object -First 1
    if ($virtual) { break }
    Start-Sleep -Milliseconds 500
}
if (-not $virtual) { throw 'no non-primary display; the virtual display was not created' }
$p = $virtual.Split(',')
$vb = [pscustomobject]@{ X = [int]$p[0]; Y = [int]$p[1]; Width = [int]$p[2]; Height = [int]$p[3] }
Step "virtual display: $($vb.Width)x$($vb.Height) at $($vb.X),$($vb.Y)"

# --------------------------------------------------------------------------------------------
# In-page instrumentation.
#
# The pointer listener goes on `window` with `capture: true`, so it runs in the capture phase -
# ahead of the page's own listener, which is registered on `#stage` in the bubble phase. A
# listener on the same element would run *after* the page's, and the hold time would come out
# negative or zero for the immediate path.
# --------------------------------------------------------------------------------------------
$install = Invoke-Js @'
(function () {
  if (window.__ilat) return 'already';
  var evs = [], sends = [];
  window.__ilat = { evs: evs, sends: sends, epoch: Date.now() - performance.now() };
  var proto = window.RTCDataChannel && RTCDataChannel.prototype;
  if (!proto || !proto.send) return 'no RTCDataChannel.prototype.send';
  var orig = proto.send;
  proto.send = function (d) {
    var op = -1, n = 0;
    try {
      var u = null;
      if (d instanceof ArrayBuffer) u = new Uint8Array(d);
      else if (d && d.buffer) u = new Uint8Array(d.buffer, d.byteOffset || 0, d.byteLength);
      if (u && u.length) { op = u[0]; n = u.length; }
    } catch (e) {}
    sends.push([performance.now(), this.label, op, n]);
    return orig.apply(this, arguments);
  };
  var type = ('onpointerrawupdate' in window) ? 'pointerrawupdate' : 'pointermove';
  window.addEventListener(type, function (e) { evs.push([performance.now(), e.isTrusted ? 1 : 0]); }, true);
  window.__ilatReset = function () { evs.length = 0; sends.length = 0; };
  // Synthetic pointer moves at a chosen cadence. Synthetic rather than driven over CDP because a
  // CDP round trip is a millisecond of its own and cannot hold 1 kHz; and `perTick` > 1 reproduces
  // the burst a high-rate mouse delivers inside one task.
  window.__sweep = function (count, gapMs, perTick) {
    var stage = document.getElementById('stage');
    var r = stage.getBoundingClientRect();
    var cx = r.left + r.width / 2, cy = r.top + r.height / 2;
    var rad = Math.min(r.width, r.height) * 0.33;
    var i = 0;
    return new Promise(function (resolve) {
      function tick() {
        for (var k = 0; k < perTick; k++) {
          var a = (i * perTick + k) * 0.11;
          var e = new PointerEvent(type, {
            bubbles: true, cancelable: true, composed: true,
            pointerId: 1, pointerType: 'mouse', isPrimary: true, buttons: 0,
            clientX: cx + rad * Math.cos(a), clientY: cy + rad * Math.sin(a)
          });
          stage.dispatchEvent(e);
        }
        if (++i >= count) { resolve(i * perTick); return; }
        setTimeout(tick, gapMs);
      }
      tick();
    });
  };
  window.__hold = function () {
    var e = window.__ilat.evs, s = window.__ilat.sends, out = [];
    var POINTER_OPS = { 3: 1, 9: 1, 0x54: 1 };
    var j = 0;
    for (var i = 0; i < s.length; i++) {
      if (!POINTER_OPS[s[i][2]]) continue;
      while (j + 1 < e.length && e[j + 1][0] <= s[i][0]) j++;
      if (j < e.length && e[j][0] <= s[i][0]) out.push(Math.round((s[i][0] - e[j][0]) * 1000) / 1000);
    }
    var byOp = {};
    for (var i = 0; i < s.length; i++) { var k = 'op' + s[i][2]; byOp[k] = (byOp[k] || 0) + 1; }
    return { hold: out, events: e.length, sends: s.length, byOp: byOp, epoch: window.__ilat.epoch };
  };
  // Wall-clock times of the pointer sends, for pairing against the host's input-trace.
  window.__sendWall = function () {
    var s = window.__ilat.sends, ep = window.__ilat.epoch, out = [];
    var POINTER_OPS = { 3: 1, 9: 1, 0x54: 1 };
    for (var i = 0; i < s.length; i++) if (POINTER_OPS[s[i][2]]) out.push(Math.round(ep + s[i][0]));
    return out;
  };
  window.__keyWall = function () {
    var s = window.__ilat.sends, ep = window.__ilat.epoch, out = [];
    for (var i = 0; i < s.length; i++) if (s[i][2] === 0x20) out.push(Math.round(ep + s[i][0]));
    return out;
  };
  return 'installed';
})()
'@
Step "page hooks: $install"
if ($install -notin @('installed', 'already')) { throw "could not install the page hooks: $install" }

# One row per injected event, from whichever `input-trace` shape the binary under test emits.
#
# The shipped diagnostic writes `wall_ms=` through the log bus (stdout under `serve -v`). The
# measurement patch carried in the baseline worktree writes `injected=` on stderr, because the
# baseline has no host-clock stage histograms to hang the number off. Both name the same instant —
# the wall clock immediately after `SendInput` returned — so both are read here and neither run
# needs a binary rebuilt to match the other's log format.
function Read-Trace {
    $rows = @()
    foreach ($f in @($hostLog, $hostErr)) {
        if (-not (Test-Path $f)) { continue }
        foreach ($m in (Select-String -Path $f -Pattern 'input-trace kind=(\S+).*?(?:wall_ms|injected)=(\d+)')) {
            $g = $m.Matches[0].Groups
            $line = $m.Line
            $q = [regex]::Match($line, 'queue_us=(\d+)')
            $inj = [regex]::Match($line, 'inject_us=(\d+)')
            $rows += [pscustomobject]@{
                kind     = $g[1].Value
                queueUs  = if ($q.Success) { [int]$q.Groups[1].Value } else { $null }
                injectUs = if ($inj.Success) { [int]$inj.Groups[1].Value } else { $null }
                wall     = [long]$g[2].Value
            }
        }
    }
    # Deliberately *not* returned as `, @(...)`: the caller always re-wraps with `@()`, and the
    # extra wrapper made `@(Read-Trace)` a one-element array holding the array, so `[0].wall` was
    # a whole column of stamps and the subtraction below failed on `System.Object[]`.
    return @($rows | Sort-Object wall)
}

$results = [ordered]@{}

# --------------------------------------------------------------------------------------------
# Phase 1 + 2: hold and forward, at three cadences.
# --------------------------------------------------------------------------------------------
foreach ($sweep in @(
        @{ name = 'isolated-4Hz'; count = 40; gap = 250; per = 1 },
        @{ name = 'sweep-125Hz'; count = 500; gap = 8; per = 1 },
        @{ name = 'sweep-1kHz'; count = 500; gap = 8; per = 8 })) {

    # Trace rows are selected by wall-clock window below, so no prior count is needed.
    Invoke-Js 'window.__ilatReset(), true' | Out-Null
    $t0 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $dispatched = Invoke-Js "window.__sweep($($sweep.count), $($sweep.gap), $($sweep.per))" -TimeoutMs 180000
    Start-Sleep -Milliseconds 900
    $t1 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()

    $h = (Invoke-Js 'JSON.stringify(window.__hold())') | ConvertFrom-Json
    $sendWall = JsonArray (Invoke-Js 'JSON.stringify(window.__sendWall())')
    # Only the pointer kinds: a `Resize` or a focus change injected in the same window is a real
    # event the host handled, but it is not the event this phase dispatched, and pairing it against
    # a pointer send shifts every later pair.
    $trace = @((Read-Trace) | Where-Object {
            $_.kind -match '^(pointer|batch|delta)' -and $_.wall -ge ($t0 - 50) -and $_.wall -le ($t1 + 50)
        })

    $p = Pair-Forward $sendWall $trace
    $fwd = $p.deltas

    $results[$sweep.name] = [ordered]@{
        dispatched      = $dispatched
        domEvents       = $h.events
        pointerSends    = $sendWall.Count
        hostInjections  = $trace.Count
        matched         = $p.matched
        unmatchedSends  = $p.unmatched
        sendsByOp       = $h.byOp
        holdMs          = Pct (@($h.hold | ForEach-Object { $_ }))
        forwardMs       = Pct $fwd
        hostQueueUs     = Pct (@($trace | Where-Object { $null -ne $_.queueUs } | ForEach-Object { $_.queueUs }))
        hostRecvToInjUs = Pct (@($trace | Where-Object { $null -ne $_.injectUs } | ForEach-Object { $_.injectUs }))
        injectKinds     = ($trace | Group-Object kind | ForEach-Object { "$($_.Name)=$($_.Count)" }) -join ','
    }
    Step ("{0}: {1} dom events -> {2} sends -> {3} injections; hold p50 {4} p99 {5}; forward p50 {6} p99 {7}" -f `
            $sweep.name, $h.events, $sendWall.Count, $trace.Count,
        $results[$sweep.name].holdMs.p50, $results[$sweep.name].holdMs.p99,
        $(if ($results[$sweep.name].forwardMs) { $results[$sweep.name].forwardMs.p50 } else { 'n/a' }),
        $(if ($results[$sweep.name].forwardMs) { $results[$sweep.name].forwardMs.p99 } else { 'n/a' }))
    Start-Sleep -Milliseconds 600
}

# --------------------------------------------------------------------------------------------
# Phase 3: frame production on a static desktop.
#
# Three windows, same length, nothing else running on the virtual display. The question is not
# "how fast is the encoder" but "does a pointer moving over an unchanged desktop produce frames at
# all" - and whether the answer differs for the client's pointer and the host's.
# --------------------------------------------------------------------------------------------
$fault = [SEIn]::SelfTest($vb.X + [int]($vb.Width / 2), $vb.Y + [int]($vb.Height / 2))
if ($fault) { throw "the pointer cannot be moved, so the host-sweep window would measure a still desktop: $fault" }

function Painted {
    $s = (Invoke-Js 'JSON.stringify(SEVideo.getStats())') | ConvertFrom-Json
    return [pscustomobject]@{
        painted     = [long]$s.framesPainted
        dropped     = [long]$s.framesDropped
        hostToPaint = $s.hostToPaint
        keyRequests = [long]$s.keyRequests
    }
}

function Window-Fps($name, $startAction, $stopAction) {
    & $startAction
    Start-Sleep -Milliseconds 700
    $a = Painted
    $sw = [Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Milliseconds ($Window * 1000)
    $secs = $sw.Elapsed.TotalSeconds
    $b = Painted
    & $stopAction
    $fps = [Math]::Round(($b.painted - $a.painted) / $secs, 2)
    Step ("frames/{0}: {1} painted in {2:N1} s = {3} fps (host capture->paint p50 {4} us)" -f `
            $name, ($b.painted - $a.painted), $secs, $fps,
        $(if ($b.hostToPaint) { $b.hostToPaint.p50 } else { '-' }))
    return [pscustomobject]@{
        fps         = $fps
        frames      = ($b.painted - $a.painted)
        seconds     = [Math]::Round($secs, 2)
        dropped     = ($b.dropped - $a.dropped)
        keyRequests = ($b.keyRequests - $a.keyRequests)
        hostToPaint = $b.hostToPaint
    }
}

# Does a single client pointer move, on a desktop where nothing else is changing, produce a frame?
#
# This is the structural question behind "the client pointer feels laggy": a host that only
# encodes when the *desktop* changes has nothing to send for a cursor that moved, and the picture
# then waits for whatever emits next. Counted rather than timed — one isolated move every `gapMs`,
# and the painted count either tracks the moves or it tracks the keepalive.
function Isolated-Moves($name, $moves, $gapMs, $mover) {
    Start-Sleep -Milliseconds 600
    $a = Painted
    $sw = [Diagnostics.Stopwatch]::StartNew()
    for ($i = 0; $i -lt $moves; $i++) {
        & $mover $i
        Start-Sleep -Milliseconds $gapMs
    }
    $secs = $sw.Elapsed.TotalSeconds
    $b = Painted
    $frames = $b.painted - $a.painted
    Step ("isolated/{0}: {1} moves over {2:N1} s -> {3} frames ({4} per move)" -f `
            $name, $moves, $secs, $frames, [Math]::Round($frames / [Math]::Max($moves, 1), 2))
    return [pscustomobject]@{
        moves        = $moves
        gapMs        = $gapMs
        seconds      = [Math]::Round($secs, 2)
        frames       = $frames
        framesPerMove = [Math]::Round($frames / [Math]::Max($moves, 1), 2)
        hostToPaint  = $b.hostToPaint
    }
}

# Park the host pointer off the virtual display for the two windows that must not have it moving.
$parkX = 10; $parkY = 10
[SEIn]::MoveTo($parkX, $parkY)
Start-Sleep -Milliseconds 400

$static = Window-Fps 'static' { } { }

$clientSweep = Window-Fps 'client-pointer' {
    Invoke-Js 'window.__sweepJob = window.__sweep(4000, 8, 1), true' | Out-Null
} {
    Invoke-Js 'true' | Out-Null
}
# The sweep promise is still resolving; let it finish so it cannot bleed into the next window.
Start-Sleep -Milliseconds 1500
Invoke-Js 'window.__ilatReset(), true' | Out-Null

$hostJob = $null
$hostSweep = Window-Fps 'host-pointer' {
    $script:hostJob = Start-Job -ArgumentList $vb.X, $vb.Y, $vb.Width, $vb.Height, ($Window + 4) -ScriptBlock {
        param($bx, $by, $bw, $bh, $secs)
        Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class SEIn3 {
    [StructLayout(LayoutKind.Sequential)]
    struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr extra; }
    [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public MOUSEINPUT mi; }
    [DllImport("user32.dll", SetLastError = true)] static extern uint SendInput(uint n, INPUT[] p, int cb);
    [DllImport("user32.dll")] static extern int GetSystemMetrics(int i);
    public static void MoveTo(int x, int y) {
        int vx = GetSystemMetrics(76), vy = GetSystemMetrics(77);
        int vw = GetSystemMetrics(78), vh = GetSystemMetrics(79);
        if (vw <= 1) vw = 1; if (vh <= 1) vh = 1;
        INPUT[] inp = new INPUT[1]; inp[0].type = 0;
        inp[0].mi.dx = (int)(((double)(x - vx) * 65535.0) / (vw - 1));
        inp[0].mi.dy = (int)(((double)(y - vy) * 65535.0) / (vh - 1));
        inp[0].mi.dwFlags = 0x0001 | 0x8000 | 0x4000;
        SendInput(1, inp, Marshal.SizeOf(typeof(INPUT)));
    }
}
'@
        $cx = $bx + [int]($bw / 2); $cy = $by + [int]($bh / 2)
        $rx = [int]($bw * 0.33); $ry = [int]($bh * 0.33)
        $sw = [Diagnostics.Stopwatch]::StartNew()
        while ($sw.Elapsed.TotalSeconds -lt $secs) {
            $a = $sw.Elapsed.TotalSeconds * 1.2 * [Math]::PI
            [SEIn3]::MoveTo($cx + [int]($rx * [Math]::Cos($a)), $cy + [int]($ry * [Math]::Sin($a)))
            Start-Sleep -Milliseconds 8
        }
    }
} {
    if ($script:hostJob) { Stop-Job $script:hostJob -EA SilentlyContinue; Remove-Job $script:hostJob -Force -EA SilentlyContinue }
}
[SEIn]::MoveTo($parkX, $parkY)
Start-Sleep -Milliseconds 500

# One move at a time, 300 ms apart: far enough that a frame produced *for* the move is
# unambiguous, and 300 ms is longer than the 200 ms keepalive so the baseline of unrelated
# emissions is known from the static window above.
$isoClient = Isolated-Moves 'client-pointer' 30 300 {
    param($i)
    Invoke-Js "(function(){var s=document.getElementById('stage'),r=s.getBoundingClientRect();
      var a=$i*0.4, rad=Math.min(r.width,r.height)*0.3;
      s.dispatchEvent(new PointerEvent(('onpointerrawupdate' in window)?'pointerrawupdate':'pointermove',
        {bubbles:true,cancelable:true,composed:true,pointerId:1,pointerType:'mouse',isPrimary:true,buttons:0,
         clientX:r.left+r.width/2+rad*Math.cos(a),clientY:r.top+r.height/2+rad*Math.sin(a)}));
      return 1})()" | Out-Null
}
$isoHost = Isolated-Moves 'host-pointer' 30 300 {
    param($i)
    $a = $i * 0.4
    $rad = [int]([Math]::Min($vb.Width, $vb.Height) * 0.3)
    [SEIn]::MoveTo($vb.X + [int]($vb.Width / 2) + [int]($rad * [Math]::Cos($a)),
        $vb.Y + [int]($vb.Height / 2) + [int]($rad * [Math]::Sin($a)))
}
[SEIn]::MoveTo($parkX, $parkY)

$results['frames'] = [ordered]@{
    windowSec           = $Window
    static              = $static
    clientPointerSweep  = $clientSweep
    hostPointerSweep    = $hostSweep
    isolatedClientMoves = $isoClient
    isolatedHostMoves   = $isoHost
}

# --------------------------------------------------------------------------------------------
# Phase 4: keystrokes. Dispatched into the page, so they take the same reliable-channel path a
# real keypress does, and paired against the host's KEY injections.
# --------------------------------------------------------------------------------------------
# Trace rows are selected by wall-clock window below, so no prior count is needed.
Invoke-Js 'window.__ilatReset(), true' | Out-Null
$kt0 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
Invoke-Js @'
(function () {
  var i = 0, letters = 'abcdefghijklmnopqrstuvwxyz';
  return new Promise(function (resolve) {
    function tick() {
      var ch = letters[i % 26];
      var init = { bubbles: true, cancelable: true, composed: true, key: ch, code: 'Key' + ch.toUpperCase(), keyCode: ch.toUpperCase().charCodeAt(0) };
      window.dispatchEvent(new KeyboardEvent('keydown', init));
      window.dispatchEvent(new KeyboardEvent('keyup', init));
      if (++i >= 60) { resolve(i); return; }
      setTimeout(tick, 60);
    }
    tick();
  });
})()
'@ -TimeoutMs 60000 | Out-Null
Start-Sleep -Milliseconds 900
$kt1 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$keyWall = JsonArray (Invoke-Js 'JSON.stringify(window.__keyWall())')
$keyTrace = @((Read-Trace) | Where-Object { $_.kind -eq 'key' -and $_.wall -ge ($kt0 - 50) -and $_.wall -le ($kt1 + 50) })
$kp = Pair-Forward $keyWall $keyTrace
$kfwd = $kp.deltas
$results['keys'] = [ordered]@{
    sends           = $keyWall.Count
    injections      = $keyTrace.Count
    unmatchedSends  = $kp.unmatched
    forwardMs       = Pct $kfwd
    hostRecvToInjUs = Pct (@($keyTrace | Where-Object { $null -ne $_.injectUs } | ForEach-Object { $_.injectUs }))
}
Step ("keys: {0} sends -> {1} injections; forward p50 {2} p99 {3}" -f $keyWall.Count, $keyTrace.Count,
    $(if ($results['keys'].forwardMs) { $results['keys'].forwardMs.p50 } else { 'n/a' }),
    $(if ($results['keys'].forwardMs) { $results['keys'].forwardMs.p99 } else { 'n/a' }))

# --------------------------------------------------------------------------------------------
# Wrap up
# --------------------------------------------------------------------------------------------
$final = $null
try { $final = (Invoke-Js 'JSON.stringify(SEVideo.getStats())') | ConvertFrom-Json } catch { }

# The host's own counters, over the control server. `frames_skipped_cadence` and
# `frames_skipped_unchanged` are the two gates that can throw away the frame carrying a moved
# cursor or a new glyph; `input_stale_drops` and `input_reorder_drops` are pointer frames the
# injector discarded. A baseline build has none of these keys, which is itself the comparison.
$hostStatus = $null
try {
    $raw = & $Binary status --json 2>$null
    if ($raw) { $hostStatus = ($raw -join '') | ConvertFrom-Json }
} catch { Step "could not read host status: $_" }

$summary = [ordered]@{
    label   = $Label
    binary  = $Binary
    when    = (Get-Date).ToString('s')
    load    = [bool]$Load
    virtual = "$($vb.Width)x$($vb.Height)"
    client  = $final
    host    = $hostStatus
    phases  = $results
}
$summary.hostTelemetry = (Select-String -Path $hostLog -Pattern 'telemetry: ' | Select-Object -Last 1).Line
$summaryPath = Join-Path $runDir 'summary.json'
$summary | ConvertTo-Json -Depth 12 | Set-Content -Encoding UTF8 $summaryPath

foreach ($j in $loadJobs) { Stop-Job $j -EA SilentlyContinue; Remove-Job $j -Force -EA SilentlyContinue }
Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -like '*se-measure*' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
if ($hostProc -and -not $hostProc.HasExited) {
    $quit = Start-Process -FilePath $Binary -PassThru -WindowStyle Hidden -ArgumentList @('stop')
    if (-not $quit.WaitForExit(8000)) { Stop-Process -Id $quit.Id -Force -EA SilentlyContinue }
    if (-not $hostProc.WaitForExit(8000)) {
        Write-Warning 'the host did not stop on request; killing it'
        Stop-Process -Id $hostProc.Id -Force -EA SilentlyContinue
    }
}
Restore-Config
Remove-Item Env:\SE_INPUT_TRACE -EA SilentlyContinue
Start-OwnerGui

Write-Host ''
Write-Host "=== $Label$(if ($Load) { ' (under load)' }) ==="
foreach ($k in @('isolated-4Hz', 'sweep-125Hz', 'sweep-1kHz')) {
    $r = $results[$k]
    if (-not $r) { continue }
    Write-Host ("{0,-13} events {1,5} sends {2,5} injected {3,5} | hold p50 {4,6} p95 {5,6} p99 {6,6} ms | forward p50 {7,5} p95 {8,5} p99 {9,5} ms" -f `
            $k, $r.domEvents, $r.pointerSends, $r.hostInjections,
        $r.holdMs.p50, $r.holdMs.p95, $r.holdMs.p99,
        $(if ($r.forwardMs) { $r.forwardMs.p50 } else { '-' }), $(if ($r.forwardMs) { $r.forwardMs.p95 } else { '-' }), $(if ($r.forwardMs) { $r.forwardMs.p99 } else { '-' }))
}
$f = $results['frames']
Write-Host ("frames  static {0} fps | CLIENT sweep {1} fps | HOST sweep {2} fps" -f `
        $f.static.fps, $f.clientPointerSweep.fps, $f.hostPointerSweep.fps)
Write-Host ("isolated 30 moves @300ms: CLIENT {0} frames ({1}/move) | HOST {2} frames ({3}/move)" -f `
        $f.isolatedClientMoves.frames, $f.isolatedClientMoves.framesPerMove,
    $f.isolatedHostMoves.frames, $f.isolatedHostMoves.framesPerMove)
if ($f.static.hostToPaint) {
    Write-Host ("host capture -> client paint (us)  p50 {0}  p90 {1}  p99 {2}" -f `
            $f.static.hostToPaint.p50, $f.static.hostToPaint.p90, $f.static.hostToPaint.p99)
}
$loc = (Select-String -Path $hostLog -Pattern 'locality=' | Select-Object -First 1)
if ($loc) { Write-Host ("session transport: " + ($loc.Line -replace '^.*?(locality=)', '$1')) }
$cur = (Select-String -Path $hostLog -Pattern '^.*cursor: ' | Select-Object -First 1)
if ($cur) { Write-Host ("cursor tier: " + ($cur.Line -replace '^.*cursor: ', '')) }
$fpsLine = (Select-String -Path $hostLog -Pattern 'fps' | Select-Object -First 3)
foreach ($l in $fpsLine) { Write-Host ("host: " + $l.Line) }
$kk = $results['keys']
Write-Host ("keys    {0} sends -> {1} injections | forward p50 {2} p99 {3} ms" -f $kk.sends, $kk.injections,
    $(if ($kk.forwardMs) { $kk.forwardMs.p50 } else { '-' }), $(if ($kk.forwardMs) { $kk.forwardMs.p99 } else { '-' }))
Write-Host "-> $summaryPath"
Stop-Transcript | Out-Null
$summaryPath
