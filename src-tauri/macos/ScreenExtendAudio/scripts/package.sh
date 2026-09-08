#!/usr/bin/env bash
# Wrap build/ScreenExtendAudio.driver in an installer package.
#
# Usually driven by scripts/release.sh.
#
# Usage:
#   ./scripts/build.sh                                        # build the .driver first
#   SE_INSTALLER_IDENTITY="Developer ID Installer: …" ./scripts/package.sh
#   SE_ALLOW_UNSIGNED=1 ./scripts/package.sh                   # dev only, see below
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="${HERE}/build"
DRIVER="${BUILD}/ScreenExtendAudio.driver"

die() { echo "error: $*" >&2; exit 1; }

[ -d "${DRIVER}" ] || die "${DRIVER} not found — run scripts/build.sh first"
[ -x /usr/libexec/PlistBuddy ] || die "/usr/libexec/PlistBuddy is missing"
command -v pkgbuild >/dev/null 2>&1 || die "pkgbuild is not on PATH — install the Command Line Tools"
command -v productbuild >/dev/null 2>&1 || die "productbuild is not on PATH"

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "${HERE}/Info.plist")"
PKG="${BUILD}/ScreenExtendAudio-${VERSION}.pkg"

INSTALLER_IDENTITY="${SE_INSTALLER_IDENTITY:-${INSTALLER_ID:-}}"
if [ -z "${INSTALLER_IDENTITY}" ] && [ -z "${SE_ALLOW_UNSIGNED:-}" ]; then
  die "no installer identity. Set SE_INSTALLER_IDENTITY, or SE_ALLOW_UNSIGNED=1 for a local
       build — an unsigned, unnotarized HAL plug-in only loads on a Mac with SIP disabled."
fi

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
if [ -n "${INSTALLER_IDENTITY}" ]; then
  productbuild --package "${COMPONENT}" --sign "${INSTALLER_IDENTITY}" "${PKG}"
else
  productbuild --package "${COMPONENT}" "${PKG}"
  echo "warning: built an UNSIGNED .pkg. It installs, but coreaudiod will refuse to load the"
  echo "         plug-in unless SIP is disabled. Do not ship this."
fi

rm -f "${COMPONENT}"
echo "Built: ${PKG}"
