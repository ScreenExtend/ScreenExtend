#!/usr/bin/env bash
# Compile libASPL + src/ into build/ScreenExtendAudio.driver.
#
# Usually driven by scripts/release.sh; run it directly for a fast edit-compile loop.
#
# Usage:
#   ./scripts/fetch-libaspl.sh && ./scripts/build.sh    # x86_64, unsigned
#   SE_ARCHS="x86_64 arm64" ./scripts/build.sh          # universal (needs an SDK that can)
#   SE_SIGN_IDENTITY="Developer ID Application: …" ./scripts/build.sh
#   SE_CLEAN=1 ./scripts/build.sh                       # discard the object cache first
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="${HERE}/build"
OUT="${BUILD}/ScreenExtendAudio.driver"

die() { echo "error: $*" >&2; exit 1; }

# locate libASPL
LIBASPL_DIR="${SE_LIBASPL_DIR:-${LIBASPL_DIR:-}}"
if [ -z "${LIBASPL_DIR}" ] && [ -d "${HERE}/third_party/libASPL/include/aspl" ]; then
  LIBASPL_DIR="${HERE}/third_party/libASPL"
fi
if [ -z "${LIBASPL_DIR}" ] || [ ! -d "${LIBASPL_DIR}/include/aspl" ]; then
  die "libASPL not found. Run ./scripts/fetch-libaspl.sh, or set SE_LIBASPL_DIR to a checkout."
fi
echo "libASPL: ${LIBASPL_DIR}"

command -v xcrun >/dev/null 2>&1 || die "xcrun is not on PATH — install the Command Line Tools"
SDK="$(xcrun --show-sdk-path 2>/dev/null)" || die "xcrun --show-sdk-path failed — no SDK selected"
[ -d "${SDK}" ] || die "the SDK path xcrun reported is not a directory: ${SDK}"
CXX="$(xcrun -f clang++ 2>/dev/null)" || die "xcrun -f clang++ failed — no toolchain"
[ -x "${CXX}" ] || die "clang++ is not executable: ${CXX}"
echo "SDK: ${SDK}"

ARCHS="${SE_ARCHS:-${ARCHS:-x86_64}}"
for a in ${ARCHS}; do
  # a 10.15 SDK cannot target arm64; find that out here rather than 30 files into the build
  if ! echo 'int main(){return 0;}' \
    | "${CXX}" -x c++ -arch "${a}" -isysroot "${SDK}" -mmacosx-version-min=10.15 \
        -o /dev/null - >/dev/null 2>&1; then
    die "this SDK cannot target ${a} (${SDK}). Build the universal driver on a newer Mac or in CI."
  fi
done
echo "architectures: ${ARCHS}"

ARCH_FLAGS=""
for a in ${ARCHS}; do ARCH_FLAGS="${ARCH_FLAGS} -arch ${a}"; done

CXXFLAGS="-std=c++17 -fPIC -Os -fvisibility=hidden -Wall ${ARCH_FLAGS} -isysroot ${SDK} \
  -I${LIBASPL_DIR}/include -I${HERE}/src -mmacosx-version-min=10.15"

if [ -n "${SE_CLEAN:-}" ]; then rm -rf "${BUILD}"; fi
rm -rf "${OUT}"
mkdir -p "${BUILD}/obj"
# the object cache is only valid for one set of flags
FLAGSTAMP="${BUILD}/obj/.flags"
if [ ! -f "${FLAGSTAMP}" ] || [ "$(cat "${FLAGSTAMP}")" != "${CXXFLAGS}" ]; then
  rm -rf "${BUILD}/obj"
  mkdir -p "${BUILD}/obj"
  printf '%s' "${CXXFLAGS}" > "${FLAGSTAMP}"
fi

compile() { # <source> <object>
  if [ -f "$2" ] && [ "$2" -nt "$1" ]; then return 0; fi
  # shellcheck disable=SC2086
  "${CXX}" ${CXXFLAGS} -c "$1" -o "$2"
}

echo "Compiling libASPL…"
for src in "${LIBASPL_DIR}"/src/*.cpp; do
  compile "${src}" "${BUILD}/obj/aspl_$(basename "${src}" .cpp).o"
done

echo "Compiling ScreenExtendAudio…"
for src in "${HERE}"/src/*.cpp; do
  compile "${src}" "${BUILD}/obj/se_$(basename "${src}" .cpp).o"
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

# coreaudiod loads as _coreaudiod; a non-world-readable file (e.g. mode 0600) is silently skipped
chmod -R a+rX "${OUT}"
chmod 0644 "${OUT}/Contents/Info.plist"
chmod 0755 "${OUT}/Contents/MacOS/ScreenExtendAudio"

SIGN_ID="${SE_SIGN_IDENTITY:-${CODESIGN_ID:-}}"
if [ -n "${SIGN_ID}" ]; then
  echo "Signing with ${SIGN_ID}…"
  codesign --force --options runtime --timestamp -s "${SIGN_ID}" "${OUT}"
  codesign --verify --deep --strict --verbose=2 "${OUT}"
fi

echo "Built: ${OUT}"
