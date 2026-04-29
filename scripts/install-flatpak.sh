#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="${ROOT_DIR}/flatpak/io.github.ashot.App.json"
BUILD_DIR="${ROOT_DIR}/build-flatpak"
ASHOT_VERSION="${ASHOT_VERSION:-$(git -C "${ROOT_DIR}" describe --tags --exact-match 2>/dev/null || true)}"
if [ -z "${ASHOT_VERSION}" ]; then
  SHORT_COMMIT="$(git -C "${ROOT_DIR}" rev-parse --short=7 HEAD 2>/dev/null || true)"
  ASHOT_VERSION="dev-${SHORT_COMMIT:-unknown}"
fi
BUNDLE_PATH="${BUILD_DIR}/io.github.ashot.App-${ASHOT_VERSION}.flatpak"

if ! command -v flatpak-builder >/dev/null 2>&1; then
  echo "flatpak-builder is required. Install it with your distribution package manager." >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required to prepare the Flatpak vendor directory." >&2
  exit 1
fi

cargo vendor --locked "${ROOT_DIR}/flatpak/vendor" \
  | sed 's#directory = ".*flatpak/vendor"#directory = "vendor"#' \
  >"${ROOT_DIR}/flatpak/cargo-config.toml"

flatpak-builder \
  --user \
  --install \
  --force-clean \
  --disable-rofiles-fuse \
  --install-deps-from=flathub \
  --repo="${BUILD_DIR}/repo" \
  -y \
  "${BUILD_DIR}" \
  "${MANIFEST}"

flatpak override \
  --user \
  --own-name=io.github.ashot.Service \
  io.github.ashot.App

flatpak build-bundle \
  "${BUILD_DIR}/repo" \
  "${BUNDLE_PATH}" \
  io.github.ashot.App

echo
echo "Installed io.github.ashot.App for the current user."
echo "Bundle: ${BUNDLE_PATH}"
echo "Run: flatpak run io.github.ashot.App gui"
