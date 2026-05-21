#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UUID="ashot-pin@io.github.ashot"
SOURCE_DIR="${ROOT_DIR}/gnome-shell/extensions/${UUID}"
DATA_HOME="${XDG_DATA_HOME:-${HOME}/.local/share}"
TARGET_DIR="${DATA_HOME}/gnome-shell/extensions/${UUID}"

if [ ! -d "${SOURCE_DIR}" ]; then
  echo "GNOME Shell extension source directory not found: ${SOURCE_DIR}" >&2
  exit 1
fi

if ! command -v gnome-extensions >/dev/null 2>&1; then
  echo "gnome-extensions is required to enable the aShot pin extension." >&2
  exit 1
fi

if ! command -v gnome-service-client >/dev/null 2>&1; then
  echo "Warning: gnome-service-client was not found. Tagged pin windows require GNOME 50/Mutter support." >&2
fi

install -d "$(dirname "${TARGET_DIR}")"
rm -rf "${TARGET_DIR}"
cp -R "${SOURCE_DIR}" "${TARGET_DIR}"

gnome-extensions enable "${UUID}"

echo "Installed and enabled ${UUID}."
echo "If the extension does not take effect immediately, log out and back in."
