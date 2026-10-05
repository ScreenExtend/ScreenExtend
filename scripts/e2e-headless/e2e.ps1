# End-to-end audio+video test: headless host from the CLI, a fresh headless Chrome, one join,
# 20 s of measured video and a 440 Hz tone through the audio path, then everything shut down.
# No input is injected and the cursor is never touched. See TEST-GUIDE.md.
param(
  [string]$Exe = "$env:USERPROFILE\.cargo\shared_target\ScreenExtend\release\ScreenExtend.exe",
  [switch]$Headed,     # a visible Chrome window instead of headless
  [switch]$NoAudio     # video only: no tone is played and config.json is never written
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $PSCommandPath
$cfg = Join-Path $env:APPDATA 'app.screenextend.desktop\config.json'
$work = Join-Path $env:TEMP 'screenextend-e2e'; New-Item -ItemType Directory -Force $work | Out-Null
if (-not (Test-Path $Exe)) { throw "host binary not found: $Exe (build it with: pnpm tauri build)" }
if (Get-Process ScreenExtend -ErrorAction SilentlyContinue) { throw 'ScreenExtend is already running. Quit it first (ScreenExtend stop); this test starts its own host.' }

$alphabet = '23456789ABCDEFGHJKLMNPQRSTUVWXYZ'.ToCharArray()
$sid = -join (1..12 | ForEach-Object { $alphabet | Get-Random })
$otp = '{0:D6}' -f (Get-Random -Maximum 1000000)
$cfgHash = if (Test-Path $cfg) { (Get-FileHash $cfg -Algorithm SHA256).Hash } else { $null }
if ($cfgHash) { Copy-Item $cfg (Join-Path $work 'config.json.before') -Force }
$log = Join-Path $work 'serve.log'; $outFile = Join-Path $work 'run.out'
$code = 1
try {
  # Credentials go by environment, never on the command line.
  $env:SCREENEXTEND_SESSION_ID = $sid; $env:SCREENEXTEND_OTP = $otp
  $hostProc = Start-Process $Exe -ArgumentList 'serve', '--no-cloud', '--no-qr', '-v' -RedirectStandardOutput $log -RedirectStandardError (Join-Path $work 'serve.err') -WindowStyle Hidden -PassThru
  $ip = $null
  foreach ($i in 1..60) {
    Start-Sleep -Milliseconds 500
    $m = Select-String -Path $log -Pattern 'https://(\d+\.\d+\.\d+\.\d+):8443' -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($m) { $ip = $m.Matches[0].Groups[1].Value; break }
    if ($hostProc.HasExited) { throw "the host exited during startup; see $log" }
  }
  if (-not $ip) { throw "the host never printed an HTTPS address; see $log" }
  "host up: pid $($hostProc.Id), https://${ip}:8443, session $sid"

  $env:SE_EXE = $Exe; $env:CHROME_FLAGS = '--mute-audio'
  $env:AUDIO = if ($NoAudio) { '' } else { '1' }; $env:HEADED = if ($Headed) { '1' } else { '' }
  node (Join-Path $here 'run.mjs') $ip $sid $otp *> $outFile
  $code = $LASTEXITCODE
  $text = Get-Content $outFile -Raw
  $i = $text.IndexOf("RESULT {")
  if ($i -ge 0) { $text.Substring($i, $text.IndexOf("`n}", $i) + 2 - $i) } else { 'no RESULT block; the run failed before measuring:'; Select-String -Path $outFile -Pattern '^(FAILED|STEP)' | ForEach-Object Line }
}
finally {
  & $Exe stop | Out-Null
  foreach ($i in 1..40) { if (-not (Get-Process ScreenExtend -ErrorAction SilentlyContinue)) { break }; Start-Sleep -Milliseconds 250 }
  if ($cfgHash -and (Get-FileHash $cfg -Algorithm SHA256).Hash -ne $cfgHash) { Copy-Item (Join-Path $work 'config.json.before') $cfg -Force; 'config.json restored to its pre-test contents' }
  Add-Type -AssemblyName System.Windows.Forms
  $left = @(Get-Process ScreenExtend -ErrorAction SilentlyContinue).Count
  $chromeLeft = @(Get-CimInstance Win32_Process -Filter "name='chrome.exe'" | Where-Object { $_.CommandLine -match 'screenextend-e2e-profile' }).Count
  "cleanup: host processes left=$left, test Chrome processes left=$chromeLeft, displays attached=$([System.Windows.Forms.Screen]::AllScreens.Count)"
  "logs: $work"
}
if ($code -eq 0) { 'E2E PASS' } else { "E2E FAIL (exit $code)" }
exit $code
