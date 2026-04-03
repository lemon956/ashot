#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UUID="ashot-shell@io.github.ashot"
SOURCE_DIR="$ROOT_DIR/gnome-shell-extension/$UUID"

if [[ ! -d "$SOURCE_DIR" ]]; then
  echo "source extension directory not found: $SOURCE_DIR" >&2
  exit 1
fi

if ! command -v zip >/dev/null 2>&1; then
  echo "zip is required to package the GNOME Shell extension" >&2
  exit 1
fi

if ! command -v gnome-extensions >/dev/null 2>&1; then
  echo "gnome-extensions command not found" >&2
  exit 1
fi

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

BUNDLE_PATH="$TMP_DIR/$UUID.shell-extension.zip"

(
  cd "$SOURCE_DIR"
  zip -qr "$BUNDLE_PATH" .
)

gnome-extensions install --force "$BUNDLE_PATH"

echo "Installed shell extension bundle for $UUID"
echo
echo "Next steps:"
echo "  1. gnome-extensions enable $UUID"
echo "  2. If GNOME does not pick it up immediately, log out and log back in"
