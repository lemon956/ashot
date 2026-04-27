#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="${ROOT_DIR}/flatpak/io.github.ashot.App.json"
BUILD_DIR="${ROOT_DIR}/build-flatpak"

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
  -y \
  "${BUILD_DIR}" \
  "${MANIFEST}"

flatpak override \
  --user \
  --own-name=io.github.ashot.Service \
  io.github.ashot.App

echo
echo "Installed io.github.ashot.App for the current user."
echo "Run: flatpak run io.github.ashot.App gui"
