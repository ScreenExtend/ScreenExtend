#!/usr/bin/env bash
#
# Build ScreenExtendAudio.driver — the Core Audio HAL plug-in (AudioServerPlugIn).
#
# We build with clang directly rather than CMake/Xcode so the driver compiles on a bare Command
# Line Tools install (the ScreenExtend dev box has no Xcode). CMakeLists / project.yml are provided
# too for contributors who prefer those; this script is the canonical, dependency-light path.
#
# libASPL (MIT) is the driver's foundation (PRD §2). For a shipped build it should be VENDORED into
# third_party/libASPL; for local dev this script falls back to the gitignored reference checkout at
# ../../.sources-macos-legacy-audio/repos/libASPL. Point LIBASPL_DIR at whichever you use.
#
# Usage:
#   ./scripts/build.sh                       # x86_64, unsigned
#   ARCHS="x86_64 arm64" ./scripts/build.sh  # universal (needs an SDK that can target arm64)
#   CODESIGN_ID="Developer ID Application: …" ./scripts/build.sh
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="${HERE}/build"
OUT="${BUILD}/ScreenExtendAudio.driver"

# ── Locate libASPL ───────────────────────────────────────────────────────────────────────────
LIBASPL_DIR="${LIBASPL_DIR:-}"
if [ -z "${LIBASPL_DIR}" ]; then
  for cand in \
    "${HERE}/third_party/libASPL" \
    "${HERE}/../../.sources-macos-legacy-audio/repos/libASPL"; do
    if [ -d "${cand}/include/aspl" ]; then LIBASPL_DIR="${cand}"; break; fi
  done
fi
if [ -z "${LIBASPL_DIR}" ] || [ ! -d "${LIBASPL_DIR}/include/aspl" ]; then
  echo "error: libASPL not found. Vendor it into third_party/libASPL or set LIBASPL_DIR." >&2
  echo "       (dev: run ./generate-sources-macos-legacy-audio.sh at the repo root first)" >&2
  exit 1
fi
echo "libASPL: ${LIBASPL_DIR}"

SDK="$(xcrun --show-sdk-path)"
ARCHS="${ARCHS:-x86_64}"
ARCH_FLAGS=""
for a in ${ARCHS}; do ARCH_FLAGS="${ARCH_FLAGS} -arch ${a}"; done

CXX="$(xcrun -f clang++)"
CXXFLAGS="-std=c++17 -fPIC -Os -fvisibility=hidden -Wall ${ARCH_FLAGS} -isysroot ${SDK} \
  -I${LIBASPL_DIR}/include -I${HERE}/src -mmacosx-version-min=10.15"

rm -rf "${BUILD}"
mkdir -p "${BUILD}/obj"

echo "Compiling libASPL…"
for src in "${LIBASPL_DIR}"/src/*.cpp; do
  obj="${BUILD}/obj/aspl_$(basename "${src}" .cpp).o"
  # shellcheck disable=SC2086
  "${CXX}" ${CXXFLAGS} -c "${src}" -o "${obj}"
done

echo "Compiling ScreenExtendAudio…"
for src in "${HERE}"/src/*.cpp; do
  obj="${BUILD}/obj/se_$(basename "${src}" .cpp).o"
  # shellcheck disable=SC2086
  "${CXX}" ${CXXFLAGS} -c "${src}" -o "${obj}"
done

echo "Linking bundle…"
# shellcheck disable=SC2086
"${CXX}" ${ARCH_FLAGS} -isysroot "${SDK}" -mmacosx-version-min=10.15 \
  -bundle -o "${BUILD}/ScreenExtendAudio" \
  "${BUILD}"/obj/*.o \
  -framework CoreFoundation -framework CoreAudio

echo "Assembling ${OUT}…"
mkdir -p "${OUT}/Contents/MacOS"
cp "${HERE}/Info.plist" "${OUT}/Contents/Info.plist"
cp "${BUILD}/ScreenExtendAudio" "${OUT}/Contents/MacOS/ScreenExtendAudio"

# coreaudiod loads this bundle as the _coreaudiod user, so every file must be world-readable and
# the executable world-executable. (Source files can arrive mode 0600 — e.g. written over an SMB
# share — which would make coreaudiod silently skip the plug-in.)
chmod -R a+rX "${OUT}"
chmod 0644 "${OUT}/Contents/Info.plist"
chmod 0755 "${OUT}/Contents/MacOS/ScreenExtendAudio"

if [ -n "${CODESIGN_ID:-}" ]; then
  echo "Signing with ${CODESIGN_ID}…"
  codesign --force --options runtime --timestamp -s "${CODESIGN_ID}" "${OUT}"
  codesign --verify --deep --strict --verbose=2 "${OUT}"
fi

echo "Built: ${OUT}"
