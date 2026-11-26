#!/bin/bash
# Build the universal `ScreenExtend bench` executable and check it against the 10.15 floor.
#
#   scripts/build-bench-macos.sh            build both slices, lipo, verify
#   scripts/build-bench-macos.sh --verify   verify an existing target/bench/ScreenExtend-bench
#   scripts/build-bench-macos.sh --no-baseline   skip the plain x86_64 build used for the nm -u diff
#
# Output: src-tauri/target/bench/ScreenExtend-bench (arm64 at minos 11.0, x86_64 at minos 10.15),
# plus target/bench/added-undefined-symbols.txt: every undefined symbol the bench feature adds to
# the x86_64 slice relative to a plain --release x86_64 build. Each of those must exist on 10.15.
#
# Never run this while a measurement is running: a build skews encoder timings.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
CRATE="$REPO/src-tauri"
OUT="$CRATE/target/bench"
BIN="$OUT/ScreenExtend-bench"
VERIFY_ONLY=0
BASELINE=1
for a in "$@"; do
  case "$a" in
    --verify) VERIFY_ONLY=1 ;;
    --no-baseline) BASELINE=0 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

cd "$CRATE" || exit 2
mkdir -p "$OUT"
FAIL=0

if [ "$VERIFY_ONLY" -eq 0 ]; then
  if ! rustup target list --installed | grep -qx x86_64-apple-darwin; then
    echo "== rustup target add x86_64-apple-darwin"
    rustup target add x86_64-apple-darwin || exit 2
  fi
  if [ ! -f "$REPO/dist/index.html" ]; then
    echo "== dist/ missing; pnpm build (tauri_build embeds it)"
    (cd "$REPO" && pnpm build) || exit 2
  fi
  SE_BENCH_GIT_DESCRIBE="$(git -C "$REPO" describe --always --dirty --tags 2>/dev/null || echo unknown)"
  export SE_BENCH_GIT_DESCRIBE
  echo "== git: $SE_BENCH_GIT_DESCRIBE"
  echo "== arm64 slice (MACOSX_DEPLOYMENT_TARGET=11.0)"
  MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release --features bench --target aarch64-apple-darwin || exit 1
  echo "== x86_64 slice (MACOSX_DEPLOYMENT_TARGET=10.15)"
  MACOSX_DEPLOYMENT_TARGET=10.15 cargo build --release --features bench --target x86_64-apple-darwin || exit 1
  lipo -create -output "$BIN" \
    target/aarch64-apple-darwin/release/ScreenExtend \
    target/x86_64-apple-darwin/release/ScreenExtend || exit 1
  if [ "$BASELINE" -eq 1 ]; then
    echo "== baseline: plain --release x86_64 (no features) for the nm -u diff"
    MACOSX_DEPLOYMENT_TARGET=10.15 cargo build --release --target x86_64-apple-darwin \
      --target-dir "$CRATE/target/bench-baseline" || exit 1
  fi
fi

[ -f "$BIN" ] || { echo "no $BIN" >&2; exit 2; }
echo
echo "== $BIN ($(du -h "$BIN" | cut -f1))"
echo "-- lipo -archs: $(lipo -archs "$BIN")"
for arch in arm64 x86_64; do
  minos=$(otool -arch "$arch" -l "$BIN" | awk '/LC_BUILD_VERSION/{f=1} f&&/minos/{print $2; exit}')
  [ -n "$minos" ] || minos=$(otool -arch "$arch" -l "$BIN" | awk '/LC_VERSION_MIN_MACOSX/{f=1} f&&/version/{print $2; exit}')
  echo "-- $arch minos: $minos"
done
[ "$(otool -arch x86_64 -l "$BIN" | awk '/LC_BUILD_VERSION/{f=1} f&&/minos/{print $2; exit}')" = "10.15" ] || { echo "FAIL: x86_64 minos is not 10.15"; FAIL=1; }

# frameworks introduced after 10.15: a link to any of them makes dyld refuse the whole binary there
NEWER="ScreenCaptureKit|UniformTypeIdentifiers|AppTrackingTransparency|MetalFX|SensitiveContentAnalysis|CoreTransferable|BackgroundAssets|ExtensionKit|SharedWithYou|Virtualization|MetricKit|DeviceCheck|Accessibility\.framework|AppIntents|Charts|SwiftUI|Combine|CryptoKit|RealityKit|ShazamKit|GroupActivities"
for arch in arm64 x86_64; do
  echo "-- $arch linked libraries:"
  otool -arch "$arch" -L "$BIN" | tail -n +2 | sed 's/^/     /'
  if otool -arch "$arch" -L "$BIN" | grep -Eq "$NEWER"; then
    echo "FAIL: $arch links a framework absent on 10.15:"; otool -arch "$arch" -L "$BIN" | grep -E "$NEWER"; FAIL=1
  fi
done
# weak framework links are fine (dyld tolerates them); strong ones are listed above either way

BASE_BIN="$CRATE/target/bench-baseline/x86_64-apple-darwin/release/ScreenExtend"
if [ -f "$BASE_BIN" ]; then
  nm -u -arch x86_64 "$BIN" | sort -u > "$OUT/bench-x86_64.undef"
  nm -u "$BASE_BIN" | sort -u > "$OUT/baseline-x86_64.undef"
  comm -13 "$OUT/baseline-x86_64.undef" "$OUT/bench-x86_64.undef" > "$OUT/added-undefined-symbols.txt"
  echo
  echo "-- undefined symbols the bench feature adds to the x86_64 slice ($(wc -l < "$OUT/added-undefined-symbols.txt" | tr -d ' ')):"
  sed 's/^/     /' "$OUT/added-undefined-symbols.txt"
  echo "-- (removed vs baseline: $(comm -23 "$OUT/baseline-x86_64.undef" "$OUT/bench-x86_64.undef" | wc -l | tr -d ' '))"
else
  echo "-- no baseline build at $BASE_BIN; skipping the nm -u diff"
fi

SDK="$(xcrun --show-sdk-path 2>/dev/null)"
echo
echo "== check-macos-dyld.sh (x86_64 slice) against $SDK ($(xcrun --show-sdk-version 2>/dev/null))"
echo "   NOTE: the verdict is only as strong as the SDK: with a newer SDK a symbol that is in its"
echo "   .tbd stubs may still be absent from the 10.15 dylibs. The strong check needs a 10.15 SDK."
lipo -thin x86_64 -output "$OUT/ScreenExtend-bench-x86_64" "$BIN"
bash "$SCRIPT_DIR/check-macos-dyld.sh" "$OUT/ScreenExtend-bench-x86_64" "$SDK" --floor 10.15 --quiet
DYLD=$?
echo "-- check-macos-dyld.sh exit: $DYLD"
[ "$DYLD" -eq 0 ] || FAIL=1

echo
[ "$FAIL" -eq 0 ] && echo "OK: $BIN" || echo "FAILED: see above"
exit "$FAIL"
