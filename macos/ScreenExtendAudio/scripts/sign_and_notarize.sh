#!/usr/bin/env bash
#
# Sign + notarize the ScreenExtend Audio driver and its installer .pkg
# (PRD-macos-legacy-audio.md §7.7, §8.4). On modern macOS an un-notarized HAL plug-in simply will
# not load — the device never appears and there is no useful error — so this is not optional for a
# real release.
#
# Requires a Developer ID Application cert (for the .driver), a Developer ID Installer cert (for the
# .pkg), and a notarytool keychain profile.
#
#   APP_ID="Developer ID Application: Example (TEAMID)" \
#   INSTALLER_ID="Developer ID Installer: Example (TEAMID)" \
#   NOTARY_PROFILE="screenextend-notary" \
#   ./scripts/sign_and_notarize.sh
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="${HERE}/build"
DRIVER="${BUILD}/ScreenExtendAudio.driver"

: "${APP_ID:?set APP_ID to your Developer ID Application identity}"
: "${INSTALLER_ID:?set INSTALLER_ID to your Developer ID Installer identity}"
: "${NOTARY_PROFILE:?set NOTARY_PROFILE to your notarytool keychain profile}"

[ -d "${DRIVER}" ] || { echo "error: run scripts/build.sh first" >&2; exit 1; }

echo "1/4  Signing the driver bundle (hardened runtime)…"
codesign --force --options runtime --timestamp -s "${APP_ID}" "${DRIVER}"
codesign --verify --deep --strict --verbose=2 "${DRIVER}"

echo "2/4  Building the installer .pkg…"
INSTALLER_ID="${INSTALLER_ID}" "${HERE}/scripts/package.sh"
PKG="$(ls -t "${BUILD}"/ScreenExtendAudio-*.pkg | head -1)"

echo "3/4  Submitting ${PKG} to the notary service (this can take minutes)…"
xcrun notarytool submit "${PKG}" --keychain-profile "${NOTARY_PROFILE}" --wait

echo "4/4  Stapling the ticket…"
xcrun stapler staple "${PKG}"
xcrun stapler validate "${PKG}"

echo "Done: notarized + stapled ${PKG}"
