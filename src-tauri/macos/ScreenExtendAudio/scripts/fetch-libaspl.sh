#!/usr/bin/env bash
# Fetch the libASPL checkout the driver builds against, pinned to one commit.
#
# libASPL (https://github.com/gavv/libASPL, MIT) is not vendored: it is ~30 source files that
# change rarely, and pinning a commit here keeps the diff of this repo about ScreenExtend. The
# checkout lands in third_party/, which is gitignored.
#
# Usage:
#   ./scripts/fetch-libaspl.sh            # clone or update third_party/libASPL to the pin
#   SE_LIBASPL_DIR=/path/to/libASPL ...   # or point the build at your own checkout instead
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${HERE}/third_party/libASPL"

# libASPL v3.1.2
LIBASPL_REPO="${LIBASPL_REPO:-https://github.com/gavv/libASPL.git}"
LIBASPL_COMMIT="${LIBASPL_COMMIT:-47f688ed6bb637ab8b7f4b36864734b2b1f69b6b}"

command -v git >/dev/null 2>&1 || { echo "error: git is not on PATH" >&2; exit 1; }

if [ -d "${DEST}/.git" ]; then
  echo "Updating ${DEST}…"
  git -C "${DEST}" fetch --quiet origin "${LIBASPL_COMMIT}" 2>/dev/null \
    || git -C "${DEST}" fetch --quiet origin
else
  echo "Cloning libASPL into ${DEST}…"
  rm -rf "${DEST}"
  mkdir -p "$(dirname "${DEST}")"
  git clone --quiet "${LIBASPL_REPO}" "${DEST}"
fi

git -C "${DEST}" checkout --quiet --detach "${LIBASPL_COMMIT}"

got="$(git -C "${DEST}" rev-parse HEAD)"
if [ "${got}" != "${LIBASPL_COMMIT}" ]; then
  echo "error: libASPL is at ${got}, expected ${LIBASPL_COMMIT}" >&2
  exit 1
fi
[ -d "${DEST}/include/aspl" ] || { echo "error: ${DEST} has no include/aspl" >&2; exit 1; }

echo "libASPL pinned at ${LIBASPL_COMMIT} in ${DEST}"
