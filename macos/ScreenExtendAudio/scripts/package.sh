#!/usr/bin/env bash
# Usage:
#   ./scripts/build.sh                                   # build the .driver first
#   INSTALLER_ID="Developer ID Installer: …" ./scripts/package.sh
#   ./scripts/package.sh                                 # unsigned (dev only; won't load without
#                                                        # notarization on modern macOS)
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="${HERE}/build"
DRIVER="${BUILD}/ScreenExtendAudio.driver"
VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "${HERE}/Info.plist")"
PKG="${BUILD}/ScreenExtendAudio-${VERSION}.pkg"

[ -d "${DRIVER}" ] || { echo "error: ${DRIVER} not found — run scripts/build.sh first" >&2; exit 1; }

# staging root mirrors the on-disk install layout
ROOT="${BUILD}/pkgroot"
rm -rf "${ROOT}"
mkdir -p "${ROOT}/Library/Audio/Plug-Ins/HAL"
cp -R "${DRIVER}" "${ROOT}/Library/Audio/Plug-Ins/HAL/"

SCRIPTS="${BUILD}/pkgscripts"
rm -rf "${SCRIPTS}"
mkdir -p "${SCRIPTS}"
cp "${HERE}/scripts/postinstall" "${SCRIPTS}/postinstall"
chmod +x "${SCRIPTS}/postinstall"

COMPONENT="${BUILD}/ScreenExtendAudio-component.pkg"
pkgbuild \
  --root "${ROOT}" \
  --scripts "${SCRIPTS}" \
  --identifier "app.screenextend.desktop.audio" \
  --version "${VERSION}" \
  --install-location "/" \
  "${COMPONENT}"

# product archive: required for notarization, optionally installer-signed
if [ -n "${INSTALLER_ID:-}" ]; then
  productbuild --package "${COMPONENT}" --sign "${INSTALLER_ID}" "${PKG}"
else
  productbuild --package "${COMPONENT}" "${PKG}"
  echo "warning: built UNSIGNED .pkg (dev only). A HAL plug-in must be signed + notarized to load"
  echo "         on modern macOS (PRD §7.7)."
fi

rm -f "${COMPONENT}"
echo "Built: ${PKG}"
