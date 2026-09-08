#!/usr/bin/env bash
# Build, sign, package, notarize and stage the ScreenExtend Audio driver — one command.
#
#   ./scripts/release.sh
#
# Every step is skipped cleanly when the identity it needs is absent, so a developer with no
# Apple credentials gets an unsigned .pkg (loadable only with SIP disabled) and a release machine
# with all three set gets a signed, notarized, stapled one. Nothing is signed implicitly.
#
# Environment:
#   SE_SIGN_IDENTITY       Developer ID Application identity, e.g. "Developer ID Application:
#                          Example Inc (TEAMID)". Signs the .driver bundle with the hardened
#                          runtime. Absent -> the bundle is left ad-hoc signed.
#   SE_INSTALLER_IDENTITY  Developer ID Installer identity, e.g. "Developer ID Installer:
#                          Example Inc (TEAMID)". Signs the .pkg. Absent -> unsigned .pkg.
#   SE_NOTARY_PROFILE      notarytool keychain profile name, created once with
#                          `xcrun notarytool store-credentials`. Absent -> no notarization,
#                          no stapling.
#   SE_ARCHS               architectures to build, default "x86_64". Use "x86_64 arm64" for a
#                          universal driver; needs an SDK that can target arm64.
#   SE_LIBASPL_DIR         an existing libASPL checkout; default third_party/libASPL, fetched by
#                          scripts/fetch-libaspl.sh.
#   SE_SKIP_COPY=1         do not copy the finished .pkg into src-tauri/resources/.
#   SE_CLEAN=1             discard the object cache before building.
#
# The finished package is copied to src-tauri/resources/ScreenExtendAudio.pkg, which is the path
# `install_audio_driver` resolves at runtime (BaseDirectory::Resource). It must also be listed in
# tauri.macos.conf.json's bundle.resources for a built app to carry it.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="${HERE}/build"
# .../src-tauri/macos/ScreenExtendAudio -> .../src-tauri
SRC_TAURI="$(cd "${HERE}/../.." && pwd)"
STAGED="${SRC_TAURI}/resources/ScreenExtendAudio.pkg"

die() { echo "error: $*" >&2; exit 1; }
step() { echo; echo "==> $*"; }

# ---------------------------------------------------------------- preflight
# Everything is checked here, before any work, so a missing tool or a typo in an identity fails in
# seconds rather than after a full compile and a notarization round trip.
step "Preflight"

[ "$(uname -s)" = "Darwin" ] || die "the driver can only be built on macOS"
for tool in git xcrun codesign pkgbuild productbuild; do
  command -v "${tool}" >/dev/null 2>&1 || die "${tool} is not on PATH"
done
[ -x /usr/libexec/PlistBuddy ] || die "/usr/libexec/PlistBuddy is missing"
xcrun --show-sdk-path >/dev/null 2>&1 || die "no macOS SDK selected — install the Command Line Tools"

SIGN_IDENTITY="${SE_SIGN_IDENTITY:-}"
INSTALLER_IDENTITY="${SE_INSTALLER_IDENTITY:-}"
NOTARY_PROFILE="${SE_NOTARY_PROFILE:-}"

if [ -n "${SIGN_IDENTITY}" ]; then
  security find-identity -v -p codesigning | grep -qF "${SIGN_IDENTITY}" \
    || die "SE_SIGN_IDENTITY \"${SIGN_IDENTITY}\" is not in the keychain
       (\`security find-identity -v -p codesigning\` lists what is)"
fi
if [ -n "${INSTALLER_IDENTITY}" ]; then
  security find-identity -v | grep -qF "${INSTALLER_IDENTITY}" \
    || die "SE_INSTALLER_IDENTITY \"${INSTALLER_IDENTITY}\" is not in the keychain
       (\`security find-identity -v\` lists what is)"
fi
if [ -n "${NOTARY_PROFILE}" ]; then
  xcrun notarytool --help >/dev/null 2>&1 \
    || die "notarytool is unavailable (needs Xcode 13+); unset SE_NOTARY_PROFILE to skip it"
  [ -n "${INSTALLER_IDENTITY}" ] \
    || die "SE_NOTARY_PROFILE is set but SE_INSTALLER_IDENTITY is not — Apple will not notarize
       an unsigned package"
fi

echo "  driver signing:  ${SIGN_IDENTITY:-SKIPPED (no SE_SIGN_IDENTITY)}"
echo "  package signing: ${INSTALLER_IDENTITY:-SKIPPED (no SE_INSTALLER_IDENTITY)}"
echo "  notarization:    ${NOTARY_PROFILE:-SKIPPED (no SE_NOTARY_PROFILE)}"

# ---------------------------------------------------------------- libASPL
step "libASPL"
if [ -n "${SE_LIBASPL_DIR:-}" ]; then
  [ -d "${SE_LIBASPL_DIR}/include/aspl" ] || die "SE_LIBASPL_DIR has no include/aspl"
  echo "  using ${SE_LIBASPL_DIR}"
else
  "${HERE}/scripts/fetch-libaspl.sh"
fi

# ---------------------------------------------------------------- build + sign
step "Building the driver bundle"
SE_SIGN_IDENTITY="${SIGN_IDENTITY}" "${HERE}/scripts/build.sh"

# ---------------------------------------------------------------- package
step "Building the installer package"
if [ -n "${INSTALLER_IDENTITY}" ]; then
  SE_INSTALLER_IDENTITY="${INSTALLER_IDENTITY}" "${HERE}/scripts/package.sh"
else
  SE_ALLOW_UNSIGNED=1 "${HERE}/scripts/package.sh"
fi

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "${HERE}/Info.plist")"
PKG="${BUILD}/ScreenExtendAudio-${VERSION}.pkg"
[ -f "${PKG}" ] || die "package.sh did not produce ${PKG}"

# ---------------------------------------------------------------- notarize
if [ -n "${NOTARY_PROFILE}" ]; then
  step "Notarizing (this takes minutes)"
  xcrun notarytool submit "${PKG}" --keychain-profile "${NOTARY_PROFILE}" --wait
  step "Stapling the ticket"
  xcrun stapler staple "${PKG}"
  xcrun stapler validate "${PKG}"
else
  step "Notarization skipped"
  echo "  The package is not notarized. coreaudiod will only load the plug-in on a Mac with SIP"
  echo "  disabled. See docs/manual-tests/macos-audio-driver-signing.md."
fi

# ---------------------------------------------------------------- stage
if [ -z "${SE_SKIP_COPY:-}" ]; then
  step "Staging for the app bundle"
  mkdir -p "$(dirname "${STAGED}")"
  cp "${PKG}" "${STAGED}"
  echo "  ${STAGED}"
  echo "  (tauri.macos.conf.json must list \"resources/ScreenExtendAudio.pkg\" in bundle.resources)"
fi

echo
echo "Done: ${PKG}"
