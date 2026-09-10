<#
.SYNOPSIS
  Measure what a joining client actually sees: freezes, cursor stalls and painted frame rate.

.DESCRIPTION
  The probe harness (`ScreenExtend probe --live`) paints a full-frame animation and measures the
  host side of the pipeline. That is exactly the content on which every frame is "changed", so it
  cannot see a pipeline that drops frames the compositor calls unchanged, and it never involves a
  browser at all. This script measures the other end: a real `serve` host, a real Chrome join, and
  a cursor moved on the virtual display with nothing else on it - the scene the owner reported as
  "the cursor gets frozen way more often".

  Two independent oracles, because neither alone is trustworthy:

  * The client's own `SEVideo.getStats()`, sampled once a second over CDP. Gives painted frames
    per second, host-capture -> paint percentiles, keyframe requests and the degradation bits.
    Present in every build back to the start of the engagement, so before/after are comparable.
  * A 60 fps `gdigrab` recording of the Chrome window, run through ffmpeg's `mpdecimate` with the
    thresholds turned down far enough that a cursor moving by one pixel counts as a change. The
    interval between successive *changed* frames is what the eye calls a freeze; anything over
    100 ms is counted as one.

.PARAMETER Binary
  Path to a built ScreenExtend.exe.

.PARAMETER Scene
  `cursor`  - the pointer is swept across an otherwise empty virtual display. This is the scene
              that separates a pipeline which honours cursor-only motion from one that does not.
  `motion`  - the same sweep over a second Chrome window painting a full-frame 60 fps animation,
              so every frame carries a real content change as well.

.EXAMPLE
  scripts/measure-stream.ps1 -Binary C:\...\ScreenExtend.exe -Label after -Scene cursor -Seconds 30
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$Label,
    [ValidateSet('cursor', 'motion')][string]$Scene = 'cursor',
    [int]$Seconds = 30,
    [int]$HttpPort = 8080,
    [int]$HttpsPort = 8443,
    [switch]$Http,
    [int]$CdpPort = 9333,
    [string]$OutDir = "$env:TEMP\se-measure",
    [switch]$NoRecord
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms

if (-not (Test-Path $Binary)) { throw "no binary at $Binary" }
$null = New-Item -ItemType Directory -Force -Path $OutDir
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$runDir = Join-Path $OutDir "$Label-$Scene-$stamp"
$null = New-Item -ItemType Directory -Force -Path $runDir
Start-Transcript -Path (Join-Path $runDir 'harness.log') -Force | Out-Null
function Step($m) { Write-Host ("[{0:HH:mm:ss}] {1}" -f (Get-Date), $m) }

# --------------------------------------------------------------------------------------------
# SendInput. SetCursorPos would move the pointer too, but it is not what a mouse does: it does
# not raise WM_INPUT and it is not what the injection path under test uses.
# --------------------------------------------------------------------------------------------
if (-not ('SEInput' -as [type])) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class SEInput {
    [StructLayout(LayoutKind.Sequential)]
    struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr extra; }
    [StructLayout(LayoutKind.Sequential)]
    struct INPUT { public uint type; public MOUSEINPUT mi; public int pad0, pad1; }
    [DllImport("user32.dll", SetLastError = true)]
    static extern uint SendInput(uint n, INPUT[] p, int cb);
    const uint MOVE = 0x0001, ABSOLUTE = 0x8000, VIRTUALDESK = 0x4000;
    // Absolute coordinates are 0..65535 across the whole virtual desktop, so the caller passes
    // desktop pixels and the mapping happens here against the metrics the OS reports.
    [DllImport("user32.dll")] static extern int GetSystemMetrics(int i);
    public static void MoveTo(int x, int y) {
        int vx = GetSystemMetrics(76), vy = GetSystemMetrics(77);
        int vw = GetSystemMetrics(78), vh = GetSystemMetrics(79);
        if (vw <= 1) vw = 1; if (vh <= 1) vh = 1;
        INPUT[] inp = new INPUT[1];
        inp[0].type = 0;
        inp[0].mi.dx = (int)(((double)(x - vx) * 65535.0) / (vw - 1));
        inp[0].mi.dy = (int)(((double)(y - vy) * 65535.0) / (vh - 1));
        inp[0].mi.dwFlags = MOVE | ABSOLUTE | VIRTUALDESK;
        SendInput(1, inp, Marshal.SizeOf(typeof(INPUT)));
    }

    // System.Windows.Forms.Screen.AllScreens caches its answer for the life of the process, so a
    // display created after this script started is invisible to it. This enumerates every time.
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
# Chrome DevTools Protocol over a raw WebSocket. PowerShell 5.1 has ClientWebSocket; it has no
# JSON-RPC client, so one request/response pair is written by hand. Events arriving on the same
# socket are skipped by id, which is why the read loop is not a single ReceiveAsync.
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
    param([string]$Method, $Params = @{}, [int]$TimeoutMs = 15000)
    $script:cdpId++
    $myId = $script:cdpId
    $payload = @{ id = $myId; method = $Method; params = $Params } | ConvertTo-Json -Depth 10 -Compress
    $bytes = [Text.Encoding]::UTF8.GetBytes($payload)
    $seg = New-Object 'System.ArraySegment[byte]' -ArgumentList @(, $bytes)
    $script:cdpSocket.SendAsync($seg, 'Text', $true, [Threading.CancellationToken]::None).Wait() | Out-Null

    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $sb = New-Object Text.StringBuilder
        do {
            $buf = New-Object byte[] 131072
            $rseg = New-Object 'System.ArraySegment[byte]' -ArgumentList @(, $buf)
            $task = $script:cdpSocket.ReceiveAsync($rseg, [Threading.CancellationToken]::None)
            if (-not $task.Wait($TimeoutMs)) { throw "CDP receive timed out on $Method" }
            $r = $task.Result
            [void]$sb.Append([Text.Encoding]::UTF8.GetString($buf, 0, $r.Count))
        } while (-not $r.EndOfMessage)
        $msg = $sb.ToString() | ConvertFrom-Json
        if ($msg.PSObject.Properties.Name -contains 'id' -and $msg.id -eq $myId) {
            if ($msg.PSObject.Properties.Name -contains 'error') { throw "CDP $Method failed: $($msg.error.message)" }
            return $msg.result
        }
    }
    throw "no CDP reply to $Method"
}

function Invoke-Js {
    param([string]$Expression, [int]$TimeoutMs = 15000, [switch]$UserGesture)
    $r = Invoke-Cdp 'Runtime.evaluate' @{
        expression    = $Expression
        returnByValue = $true
        awaitPromise  = $true
        userGesture   = [bool]$UserGesture
    } $TimeoutMs
    if ($r.exceptionDetails) { throw "page threw: $($r.exceptionDetails.text)" }
    return $r.result.value
}

# --------------------------------------------------------------------------------------------
# Host
# --------------------------------------------------------------------------------------------
Get-Process ScreenExtend -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 800

# The join form validates the session id against /^[23456789ABCDEFGHJKLMNPQRSTUVWXYZ]{12}$/ and
# leaves Connect disabled for anything else - so an id the *host* accepts on the command line is
# not necessarily one the page will let you submit.
$alphabet = '23456789ABCDEFGHJKLMNPQRSTUVWXYZ'.ToCharArray()
$sessionId = -join (1..12 | ForEach-Object { $alphabet[(Get-Random -Maximum $alphabet.Length)] })
$otp = '{0:D6}' -f (Get-Random -Minimum 100000 -Maximum 999999)
$hostLog = Join-Path $runDir 'host.log'

$hostProc = Start-Process -FilePath $Binary -PassThru -WindowStyle Minimized `
    -ArgumentList @('serve', '-v', '--no-cloud', '--no-qr',
    '--http-port', "$HttpPort", '--session-id', $sessionId, '--otp', $otp) `
    -RedirectStandardOutput $hostLog -RedirectStandardError (Join-Path $runDir 'host.err.log')

# The server binds to the adapter address, not to loopback - `http://localhost:8080/health` is
# refused even while the host is up - so the address is read out of the banner it printed rather
# than assumed. curl.exe rather than Invoke-WebRequest: the latter goes through the system proxy
# resolution and ignores -TimeoutSec while it does, which turns a 60 s wait into a 40 minute one.
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
    $body = & curl.exe -s --max-time 3 "$base/health" 2>$null
    if ($body -eq 'ok') { $ready = $true; break }
    Start-Sleep -Milliseconds 500
}
if (-not $ready) { throw "host never served /health at $base (see $hostLog)" }
Step "host up: $base session=$sessionId otp=$otp"

$displaysBefore = ([SEInput]::Monitors()).Count

# --------------------------------------------------------------------------------------------
# Client
# --------------------------------------------------------------------------------------------
$chrome = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
if (-not (Test-Path $chrome)) { $chrome = 'C:\Program Files (x86)\Google\Chrome\Application\chrome.exe' }
$profileDir = Join-Path $runDir 'chrome-profile'
# HTTPS, not HTTP. `VideoDecoder` needs a secure context, so a client joined over plain HTTP is
# on the `<video>` element path with no WebCodecs at all - a different pipeline from the one under
# measurement. The certificate is the host's own self-signed one, so Chrome is told to accept it.
$joinUrl = if ($Http) { "$base/?id=$sessionId&otp=$otp&name=measure&fs=0" }
           else { "https://${serveHost}:$HttpsPort/?id=$sessionId&otp=$otp&name=measure&fs=0" }

# A measurement browser from an earlier run keeps the debugging port, and `curl /json` would then
# hand back its stale join page - which this script would drive with the wrong OTP and then wait
# on forever. Take the port back before asking for it.
Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -like "*--remote-debugging-port=$CdpPort*" } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Start-Sleep -Milliseconds 500

# Launched on about:blank and navigated over CDP, not straight at the join URL: it is the only
# way to be sure the attached target is this run's, and it is where the dialog stubs go in. A
# `window.alert` raised in a page blocks the renderer, and with it every `Runtime.evaluate` this
# script depends on - a hang with no error anywhere.
$chromeArgs = @(
    "--remote-debugging-port=$CdpPort",
    "--user-data-dir=$profileDir",
    '--autoplay-policy=no-user-gesture-required',
    '--no-first-run', '--no-default-browser-check',
    '--disable-features=Translate,MediaRouter',
    '--ignore-certificate-errors', '--allow-insecure-localhost',
    '--new-window', '--window-position=0,0', '--window-size=1280,800',
    'about:blank'
)
Step 'launching chrome'
$chromeProc = Start-Process -FilePath $chrome -PassThru -ArgumentList $chromeArgs

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
Step "cdp target $($target.url)"
Connect-Cdp $target.webSocketDebuggerUrl
Step 'cdp connected'
Invoke-Cdp 'Page.enable' @{} | Out-Null
Step 'page domain enabled'

Invoke-Cdp 'Page.navigate' @{ url = $joinUrl } | Out-Null
Step "navigated to $joinUrl"
$loaded = $false
for ($i = 0; $i -lt 40; $i++) {
    Start-Sleep -Milliseconds 500
    try { if ((Invoke-Js "document.readyState === 'complete' && !!document.getElementById('connect')") -eq $true) { $loaded = $true; break } } catch { }
}
if (-not $loaded) { throw 'the join page never finished loading' }

# The page raises a same-device warning when the request came from the host itself; it is modal
# over the join form, so it is dismissed before anything else is touched.
Start-Sleep -Milliseconds 1500
Invoke-Js @'
(function () {
  var d = document.getElementById('sameDeviceDismiss');
  if (d && d.offsetParent !== null) { d.click(); return 'dismissed'; }
  return 'absent';
})()
'@ | ForEach-Object { Step "same-device modal: $_" }

# The same-device warning is not the only modal in front of the form: the HTTPS upsell and the
# limited-browser-support card both raise themselves on load, and a button under a modal cannot
# be clicked. Dismissing whatever is up is retried, because they are raised in sequence.
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
  return JSON.stringify({ clicked: clicked, disabled: !!(c && c.disabled), err: (document.getElementById('errorMessage') || {}).textContent || '' });
})()
'@ | ConvertFrom-Json
    if ($state.clicked.Count -gt 0) { Step ("dismissed: " + ($state.clicked -join ',')) }
    if (-not $state.disabled -and $state.clicked.Count -eq 0) { break }
    Start-Sleep -Milliseconds 700
}
$pre = Invoke-Js "JSON.stringify({ disabled: document.getElementById('connect').disabled, webcodecs: 'VideoDecoder' in window, secure: window.isSecureContext, coi: !!self.crossOriginIsolated })"
Step "pre-click: $pre"
Step 'clicking connect'
Invoke-Js "document.getElementById('connect').click(), true" -UserGesture | Out-Null

# The join is only real once the client has painted; framesPainted is the client's own count of
# frames that reached a sink, not of frames it accepted.
$joined = $false
for ($i = 0; $i -lt 160; $i++) {
    Start-Sleep -Milliseconds 500
    try {
        $s = Invoke-Js 'window.SEVideo ? JSON.stringify(SEVideo.getStats()) : ""'
        if ($s) {
            $st = $s | ConvertFrom-Json
            if ($st.framesPainted -gt 0) { $joined = $true; break }
        }
    } catch { }
}
if (-not $joined) {
    $log = ''
    try { $log = Invoke-Js "(window.__log || []).slice(-40).join('
') + '
--- error: ' + ((document.getElementById('errorMessage')||{}).textContent || '')" } catch { }
    Set-Content -Encoding UTF8 -Path (Join-Path $runDir 'client-console.log') -Value $log
    Step "client console:`n$log"
    throw "client never painted a frame (host log: $hostLog)"
}
Step 'client painted its first frame'

# The display the host created for this client is the one that was not there before the join.
Start-Sleep -Milliseconds 500
$virtual = [SEInput]::Monitors() | Where-Object { $_.Split(',')[4] -eq '0' } | Select-Object -First 1
if (-not $virtual) { throw 'no non-primary display; the virtual display was not created' }
$p = $virtual.Split(',')
$vb = [pscustomobject]@{ X = [int]$p[0]; Y = [int]$p[1]; Width = [int]$p[2]; Height = [int]$p[3] }
Step "virtual display: $($vb.Width)x$($vb.Height) at $($vb.X),$($vb.Y)"

if ($Scene -eq 'motion') {
    # A second window on the virtual display painting a full-frame 60 fps animation, so every
    # captured frame carries a real content change as well as the moving cursor.
    $anim = Join-Path $runDir 'motion.html'
    @'
<!doctype html><meta charset=utf-8><title>motion</title>
<style>html,body{margin:0;height:100%;overflow:hidden;background:#111}canvas{display:block}</style>
<canvas id=c></canvas><script>
var c=document.getElementById('c'),x=c.getContext('2d'),t=0;
function r(){c.width=innerWidth;c.height=innerHeight}
addEventListener('resize',r);r();
(function f(){t+=0.016;
 for(var i=0;i<40;i++){var h=(i*9+t*60)%360;x.fillStyle='hsl('+h+',80%,50%)';
  x.fillRect((i*137+t*400)%c.width,(i*79+t*220)%c.height,180,140);}
 x.fillStyle='#fff';x.font='48px sans-serif';x.fillText(t.toFixed(2),40,80);
 requestAnimationFrame(f);})();
</script>
'@ | Set-Content -Encoding UTF8 $anim
    Start-Process -FilePath $chrome -ArgumentList @(
        "--user-data-dir=$profileDir", '--new-window',
        "--window-position=$($vb.X),$($vb.Y)", "--window-size=$($vb.Width),$($vb.Height)",
        "--app=file:///$($anim -replace '\\','/')") | Out-Null
    Start-Sleep -Seconds 3
}

# --------------------------------------------------------------------------------------------
# The sweep. A circle rather than a line so the pointer never rests and never leaves the display,
# stepped at 8 ms so the motion is finer than any plausible compose cadence.
# --------------------------------------------------------------------------------------------
$mover = Start-Job -ArgumentList $vb.X, $vb.Y, $vb.Width, $vb.Height, ($Seconds + 6) -ScriptBlock {
    param($bx, $by, $bw, $bh, $secs)
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class SEInput2 {
    [StructLayout(LayoutKind.Sequential)]
    struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr extra; }
    [StructLayout(LayoutKind.Sequential)]
    struct INPUT { public uint type; public MOUSEINPUT mi; public int pad0, pad1; }
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
}
'@
    $cx = $bx + [int]($bw / 2); $cy = $by + [int]($bh / 2)
    $rx = [int]($bw * 0.35); $ry = [int]($bh * 0.35)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $secs) {
        $a = $sw.Elapsed.TotalSeconds * 1.2 * [Math]::PI
        [SEInput2]::MoveTo($cx + [int]($rx * [Math]::Cos($a)), $cy + [int]($ry * [Math]::Sin($a)))
        Start-Sleep -Milliseconds 8
    }
}

$recording = Join-Path $runDir 'client.mkv'
$ffmpeg = $null
if (-not $NoRecord) {
    $ff = Get-Command ffmpeg -ErrorAction SilentlyContinue
    if ($ff) {
        # -draw_mouse 0 so the only pointer in the recording is the one the host composited into
        # the video. qp 0 because mpdecimate is about to be asked whether a 32x32 sprite moved.
        $ffmpeg = Start-Process -FilePath $ff.Source -PassThru -WindowStyle Hidden -ArgumentList @(
            '-y', '-f', 'gdigrab', '-framerate', '60', '-draw_mouse', '0',
            '-i', 'desktop', '-offset_x', '0', '-offset_y', '0', '-video_size', '1280x800',
            '-t', "$Seconds", '-c:v', 'libx264', '-preset', 'ultrafast', '-qp', '0',
            $recording) -RedirectStandardError (Join-Path $runDir 'ffmpeg.log')
    } else {
        Write-Warning 'ffmpeg not on PATH; the recording oracle is skipped'
    }
}

# --------------------------------------------------------------------------------------------
# Sample
# --------------------------------------------------------------------------------------------
$samples = @()
$sw = [Diagnostics.Stopwatch]::StartNew()
$prevPainted = $null
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
    Start-Sleep -Milliseconds 1000
    $row = [ordered]@{ t = [Math]::Round($sw.Elapsed.TotalSeconds, 2) }
    try {
        $st = (Invoke-Js 'JSON.stringify(SEVideo.getStats())') | ConvertFrom-Json
        $row.framesPainted = $st.framesPainted
        $row.paintedPerSec = if ($null -ne $prevPainted) { $st.framesPainted - $prevPainted } else { $null }
        $prevPainted = $st.framesPainted
        $row.framesDropped = $st.framesDropped
        $row.keyRequests = $st.keyRequests
        $row.decodeErrors = $st.decodeErrors
        $row.videoPath = $st.videoPath
        $row.renderTier = $st.renderTier
        $row.degraded = $st.degraded
        $row.rttMs = $st.rttMs
        $row.hostToPaint = $st.hostToPaint
    } catch { $row.clientError = "$_" }
    $samples += [pscustomobject]$row
}
$sw.Stop()

if ($ffmpeg) { $ffmpeg.WaitForExit(20000) | Out-Null }
Stop-Job $mover -ErrorAction SilentlyContinue
Remove-Job $mover -Force -ErrorAction SilentlyContinue

$hostStatus = $null

$final = $null
try { $final = (Invoke-Js 'JSON.stringify(SEVideo.getStats())') | ConvertFrom-Json } catch { }

# --------------------------------------------------------------------------------------------
# Freeze analysis. mpdecimate with the thresholds at their floor: `hi`/`lo` are 8x8-block SAD
# sums, so the defaults (64*12, 64*5) call a 32x32 cursor sprite moving across a still desktop a
# duplicate - which is precisely the frame this is trying to count. frac 0 means one changed
# block is enough.
# --------------------------------------------------------------------------------------------
$freeze = $null
if ($ffmpeg -and (Test-Path $recording)) {
    $info = Join-Path $runDir 'decimate.txt'
    $ffPath = (Get-Command ffmpeg).Source
    Start-Process -FilePath $ffPath -Wait -NoNewWindow -RedirectStandardError $info -ArgumentList @(
        '-v', 'info', '-i', $recording, '-vf', 'mpdecimate=hi=1:lo=1:frac=0,showinfo', '-f', 'null', '-') | Out-Null
    $times = Select-String -Path $info -Pattern 'pts_time:([0-9.]+)' -AllMatches |
        ForEach-Object { [double]$_.Matches[0].Groups[1].Value } | Sort-Object
    if ($times.Count -gt 2) {
        $gaps = @()
        for ($i = 1; $i -lt $times.Count; $i++) { $gaps += ($times[$i] - $times[$i - 1]) * 1000.0 }
        $sorted = $gaps | Sort-Object
        $span = $times[$times.Count - 1] - $times[0]
        $freeze = [ordered]@{
            changedFrames    = $times.Count
            spanSec          = [Math]::Round($span, 2)
            changedPerSec    = [Math]::Round($times.Count / [Math]::Max($span, 0.001), 1)
            gapP50Ms         = [Math]::Round($sorted[[int]($sorted.Count * 0.50)], 1)
            gapP99Ms         = [Math]::Round($sorted[[int]([Math]::Min($sorted.Count - 1, $sorted.Count * 0.99))], 1)
            gapMaxMs         = [Math]::Round(($gaps | Measure-Object -Maximum).Maximum, 1)
            freezes100ms     = ($gaps | Where-Object { $_ -ge 100 }).Count
            freezes250ms     = ($gaps | Where-Object { $_ -ge 250 }).Count
            freezesPerMin100 = [Math]::Round((($gaps | Where-Object { $_ -ge 100 }).Count) * 60.0 / [Math]::Max($span, 0.001), 1)
            stallMsPerMin    = [Math]::Round(((($gaps | Where-Object { $_ -ge 100 }) | Measure-Object -Sum).Sum) * 60.0 / [Math]::Max($span, 0.001), 0)
        }
    }
}

$painted = $samples | Where-Object { $null -ne $_.paintedPerSec } | ForEach-Object { $_.paintedPerSec }
$summary = [ordered]@{
    label        = $Label
    scene        = $Scene
    binary       = $Binary
    seconds      = $Seconds
    when         = (Get-Date).ToString('s')
    virtual      = "$($vb.Width)x$($vb.Height)"
    paintedFps   = if ($painted) { [ordered]@{
            mean = [Math]::Round((($painted | Measure-Object -Average).Average), 1)
            min  = ($painted | Measure-Object -Minimum).Minimum
            max  = ($painted | Measure-Object -Maximum).Maximum
        } } else { $null }
    clientFinal  = $final
    freeze       = $freeze
    hostStatus   = $hostStatus
    samples      = $samples
}
$summary.hostTelemetryLine = (Select-String -Path $hostLog -Pattern 'telemetry: ' | Select-Object -Last 1).Line
$summaryPath = Join-Path $runDir 'summary.json'
$summary | ConvertTo-Json -Depth 12 | Set-Content -Encoding UTF8 $summaryPath

try { Invoke-Js 'true' | Out-Null } catch { }
if ($chromeProc -and -not $chromeProc.HasExited) { Stop-Process -Id $chromeProc.Id -Force -ErrorAction SilentlyContinue }
Get-Process chrome -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $chrome -and $_.MainWindowTitle -like '*ScreenExtend*' } |
    Stop-Process -Force -ErrorAction SilentlyContinue
if ($hostProc -and -not $hostProc.HasExited) { Stop-Process -Id $hostProc.Id -Force -ErrorAction SilentlyContinue }

Write-Host ''
Write-Host "=== $Label / $Scene ==="
if ($summary.paintedFps) { Write-Host ("painted fps  mean {0}  min {1}  max {2}" -f $summary.paintedFps.mean, $summary.paintedFps.min, $summary.paintedFps.max) }
if ($freeze) {
    Write-Host ("changed frames/s {0}   gap p50 {1} ms  p99 {2} ms  max {3} ms" -f $freeze.changedPerSec, $freeze.gapP50Ms, $freeze.gapP99Ms, $freeze.gapMaxMs)
    Write-Host ("freezes >=100 ms: {0} ({1}/min), >=250 ms: {2}" -f $freeze.freezes100ms, $freeze.freezesPerMin100, $freeze.freezes250ms)
}
if ($final) {
    Write-Host ("videoPath {0} renderTier {1} degraded 0x{2} keyRequests {3} dropped {4} decodeErrors {5}" -f `
            $final.videoPath, [int]$final.renderTier, ([int]$final.degraded).ToString('x'), $final.keyRequests, $final.framesDropped, $final.decodeErrors)
    if ($final.hostToPaint) { Write-Host ("host capture -> paint  p50 {0}  p90 {1}  p99 {2} (us)" -f $final.hostToPaint.p50, $final.hostToPaint.p90, $final.hostToPaint.p99) }
}
$tel = Select-String -Path $hostLog -Pattern 'telemetry: ' | Select-Object -Last 1
if ($tel) { Write-Host ("host: " + ($tel.Line -replace '^.*telemetry: ', '')) }
Write-Host "-> $summaryPath"
$summaryPath
