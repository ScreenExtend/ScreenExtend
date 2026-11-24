#!/bin/bash
# Host launcher for the macOS real-session e2e test (run-mac.mjs). Runs on the host Mac itself:
# locally as `bash mac-host.sh ...`, remotely as `ssh <mac> bash -s -- ... < mac-host.sh` (the
# caller prepends the credential exports to stdin, so they never reach any argv).
# Bash 3.2 compatible (macOS 10.15's /bin/bash).
#
#   start  <bin> <datadir> <http-port> <https-port>   serve --no-cloud --no-qr -v, isolated in <datadir>
#   status <bin> <datadir>                            status --json against that host
#   stop   <bin> <datadir> <http-port> <https-port>   stop, wait, escalate if needed, verify nothing left
#   probe  [mover]                                    lock state, displays, ScreenExtend processes
#   move   <mover> <display-id> <seconds>             cursor motion over one display
#   devset <bin> <datadir> <token> <opts...>          devices set against that host (renegotiates live)
#   powersample <pid> <seconds> / cputime <pid>       power + host CPU time (see --power)
#   motionwin <helper> <id> <mode> <secs> <out> <direct|open>   motion-window on that display
#
# Output is KEY=value lines (and raw JSON for status/displays) for run-mac.mjs to parse.
# Credentials come from SCREENEXTEND_SESSION_ID / SCREENEXTEND_OTP in the environment.
set -u
cmd=${1:-}; shift || true

listening() { # port -> pids listening on it (empty when none)
  lsof -nP -t -iTCP:"$1" -sTCP:LISTEN 2>/dev/null | tr '\n' ' '
}
locked() {
  if ioreg -n Root -d1 | grep -q '"CGSSessionScreenIsLocked"=Yes'; then echo 1; else echo 0; fi
}

case "$cmd" in
start)
  bin=$1 dir=$2 http=$3 https=$4
  [ -x "$bin" ] || { echo "ERROR=binary not found or not executable: $bin"; exit 2; }
  [ -n "${SCREENEXTEND_SESSION_ID:-}" ] && [ -n "${SCREENEXTEND_OTP:-}" ] || { echo "ERROR=credentials missing from the environment"; exit 2; }
  for p in "$http" "$https"; do
    l=$(listening "$p"); [ -z "$l" ] || { echo "ERROR=port $p already has a listener (pid $l)"; exit 3; }
  done
  mkdir -p "$dir" && chmod 700 "$dir" || { echo "ERROR=cannot create $dir"; exit 2; }
  cd "$dir" || exit 2
  echo "LOCKED_AT_START=$(locked)"
  SCREENEXTEND_DATA_DIR="$dir" nohup "$bin" serve --no-cloud --no-qr -v \
    --http-port "$http" --https-port "$https" > "$dir/serve.log" 2>&1 < /dev/null &
  pid=$!
  echo "$pid" > "$dir/serve.pid"
  echo "PID=$pid"
  i=0
  while [ $i -lt 160 ]; do # 40 s
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "ERROR=host exited during startup"
      echo "---LOG---"; tail -40 "$dir/serve.log"; exit 4
    fi
    if grep -q "Host is running" "$dir/serve.log" 2>/dev/null && [ -n "$(listening "$https")" ]; then
      echo "READY=1"; echo "STARTUP_MS=$((i * 250))"
      # Which SE_* variables the host process really carries (names and values of those
      # only; nothing else from its environment is printed).
      echo "HOST_ENV=$(ps -Eww -o command= -p "$pid" 2>/dev/null | tr ' ' '\n' | grep '^SE_' | tr '\n' ' ')"
      exit 0
    fi
    sleep 0.25; i=$((i + 1))
  done
  echo "ERROR=host never became ready (no 'Host is running' + HTTPS listener in 40 s)"
  echo "---LOG---"; tail -40 "$dir/serve.log"; exit 5
  ;;
status)
  bin=$1 dir=$2
  SCREENEXTEND_DATA_DIR="$dir" "$bin" status --json 2>/dev/null
  ;;
stop)
  bin=$1 dir=$2 http=$3 https=$4
  pid=$(cat "$dir/serve.pid" 2>/dev/null)
  # 20 s for a clean stop, counting the `stop` command itself (which may hang with the host);
  # then the host is killed, and STOPPED_BY says so.
  SCREENEXTEND_DATA_DIR="$dir" "$bin" stop > "$dir/stop.out" 2>&1 < /dev/null & sp=$!
  how=stop; i=0
  while [ $i -lt 80 ]; do
    if ! kill -0 "$sp" 2>/dev/null && { [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; }; then break; fi
    sleep 0.25; i=$((i + 1))
  done
  echo "STOP_MS=$((i * 250))"
  if kill -0 "$sp" 2>/dev/null; then kill -KILL "$sp"; echo "STOP_EXIT=hung"; else wait "$sp"; echo "STOP_EXIT=$?"; fi
  echo "STOP_OUT=$(tr '\n' ' ' < "$dir/stop.out" 2>/dev/null | cut -c1-200)"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    how=SIGKILL
    sample "$pid" 2 -file "$dir/hang.sample" >/dev/null 2>&1
    kill -KILL "$pid" 2>/dev/null; sleep 0.5
  fi
  echo "STOPPED_BY=$how"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then echo "LEFT_PID=$pid"; else echo "LEFT_PID="; fi
  echo "LEFT_LISTENERS=$(listening "$http")$(listening "$https")"
  echo "LEFT_PROCS=$(ps -axo pid=,command= | grep -F "$bin serve" | grep -F -- "--https-port $https" | grep -v grep | awk '{print $1}' | tr '\n' ' ')"
  ;;
probe)
  mover=${1:-}
  echo "LOCKED=$(locked)"
  echo "BENCH_PROCS=$(pgrep -f ScreenExtend-bench 2>/dev/null | tr '\n' ' ')"
  echo "OS=$(sw_vers -productVersion) $(uname -m)"
  echo "POWER_SOURCE=$(pmset -g ps 2>/dev/null | head -1 | sed "s/.*from //; s/'//g")"
  if [ -n "$mover" ] && [ -x "$mover" ]; then echo "DISPLAYS=$("$mover" list 2>&1)"; fi
  ;;
move)
  "$1" move "$2" "$3"
  ;;
powersample) # <pid> <seconds>: one line a second: wall time, the process's CPU time, and the
  # Apple-silicon PowerTelemetryData System* fields (mW / mV / mA) when the machine has them.
  # They refresh about once a minute: Accumulated* is the sum of 1 Hz mW samples and *Count the
  # number of samples, so (delta Accumulated / delta Count) between two refreshes is the exact
  # mean power over that minute.
  pid=$1 n=$2; i=0
  while [ $i -lt "$n" ]; do
    t=$(date +%s)
    pw=$(ioreg -rw0 -c AppleSmartBattery 2>/dev/null | grep -o '"PowerTelemetryData" = {[^}]*}' | grep -o '"\(System[A-Za-z]*\|AccumulatedSystemPowerIn\|AccumulatedSystemLoad\)"=[0-9]*' | tr -d '"' | tr '\n' ' ')
    c=$(ps -o time= -p "$pid" 2>/dev/null | tr -d ' ')
    echo "PS t=$t cpu=$c $pw"
    sleep 1; i=$((i + 1))
  done
  ;;
fds) # <pid>: descriptor listing, threads, RSS (and mach ports when readable unprivileged)
  echo "THREADS=$(($(ps -M -p "$1" 2>/dev/null | wc -l) - 1))"
  echo "RSS_KB=$(ps -o rss= -p "$1" | tr -d ' ')"
  echo "MACH_PORTS=$(lsmp -p "$1" 2>/dev/null | grep -c '^ *0x')"
  echo "---LSOF---"
  lsof -nP -p "$1" 2>/dev/null
  ;;
cputime) # <pid>
  echo "CPU=$(ps -o time= -p "$1" 2>/dev/null | tr -d ' ')"
  ;;
motionwin) # <helper> <display-id> <mode> <seconds> <out.json> <direct|open>
  helper=$1 id=$2 mode=$3 secs=$4 out=$5 how=${6:-direct}
  rm -f "$out"
  if [ "$how" = open ]; then
    # Through LaunchServices, so the window belongs to the GUI session even from an SSH login.
    app="$(dirname "$helper")/SEMotion.app"
    mkdir -p "$app/Contents/MacOS"
    cp -f "$helper" "$app/Contents/MacOS/motion-window"
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>motion-window</string>
<key>CFBundleIdentifier</key><string>app.screenextend.e2e.motion</string>
<key>CFBundleName</key><string>SEMotion</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSUIElement</key><true/>
</dict></plist>
PLIST
    open -n -W "$app" --args "$id" "$mode" "$secs" --out "$out"
  else
    "$helper" "$id" "$mode" "$secs" --out "$out" >/dev/null 2>&1
  fi
  echo "MOTION=$(cat "$out" 2>/dev/null)"
  rm -f "$out"
  ;;
killwin) # kill any motion-window this harness started (recognised by its --out path), say how many
  n=$(pgrep -f 'motion-window .*--out /tmp/se-motion-' | wc -l | tr -d ' ')
  pkill -KILL -f 'motion-window .*--out /tmp/se-motion-'
  echo "KILLED_WINDOWS=$n"
  ;;
devset) # <bin> <datadir> <device-token> <devices-set options...>: per-device override, applied live
  bin=$1 dir=$2 token=$3; shift 3
  SCREENEXTEND_DATA_DIR="$dir" "$bin" devices set "$token" "$@" 2>&1; echo "DEVSET_EXIT=$?"
  ;;
*)
  echo "usage: mac-host.sh start|status|stop|probe|move ..." >&2; exit 64
  ;;
esac
