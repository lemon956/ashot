#!/bin/sh
set -eu

DEFAULT_REPO="${ASHOT_DEFAULT_REPO:-__ASHOT_REPO__}"
REPO="${ASHOT_REPO:-${DEFAULT_REPO}}"
VERSION="${ASHOT_VERSION:-latest}"
KIND="${1:-auto}"
EXT_UUID="ashot-pin@io.github.ashot"

if [ -z "${REPO}" ] || [ "${REPO}" = "__ASHOT_REPO__" ]; then
  cat >&2 <<'EOF'
Set ASHOT_REPO to the GitHub repository, for example:

  curl -fsSL https://raw.githubusercontent.com/owner/ashot/main/scripts/install-release-packages.sh | ASHOT_REPO=owner/ashot sh

Release assets also provide install.sh with the repository baked in:

  curl -fsSL https://github.com/owner/ashot/releases/latest/download/install.sh | sh
EOF
  exit 1
fi

need_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "$1 is required." >&2
    exit 1
  fi
}

resolve_latest_version() {
  latest_json="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest")"
  latest_tag="$(printf '%s\n' "${latest_json}" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | sed -n '1p')"
  if [ -z "${latest_tag}" ]; then
    echo "Failed to resolve latest release tag for ${REPO}." >&2
    exit 1
  fi
  VERSION="${latest_tag}"
}

detect_package_kind() {
  if command -v dnf >/dev/null 2>&1; then
    echo "rpm"
    return
  fi
  if command -v apt >/dev/null 2>&1; then
    echo "deb"
    return
  fi
  echo "Could not detect package type. Run with: sh -s -- rpm or sh -s -- deb" >&2
  exit 1
}

release_base_url() {
  printf 'https://github.com/%s/releases/download/%s\n' "${REPO}" "${VERSION}"
}

download_asset() {
  asset_name="$1"
  output_name="$2"
  url="$(release_base_url)/${asset_name}"

  echo "Downloading ${asset_name}"
  curl -fL "${url}" -o "${TMP_DIR}/${output_name}"
}

install_rpm_packages() {
  need_command sudo
  need_command dnf

  package_version="${VERSION#v}"
  rpm_version="$(printf '%s' "${package_version}" | tr '+-' '__')"

  download_asset "ashot-${rpm_version}-1.x86_64.rpm" "ashot.rpm"
  download_asset "ashot-gnome-shell-extension-${rpm_version}-1.noarch.rpm" \
    "ashot-gnome-shell-extension.rpm"
  sudo dnf install -y "${TMP_DIR}/ashot.rpm" "${TMP_DIR}/ashot-gnome-shell-extension.rpm"
}

install_deb_packages() {
  need_command sudo
  need_command apt

  package_version="${VERSION#v}"

  download_asset "ashot_${package_version}_amd64.deb" "ashot.deb"
  download_asset "ashot-gnome-shell-extension_${package_version}_all.deb" \
    "ashot-gnome-shell-extension.deb"
  sudo apt install -y "${TMP_DIR}/ashot.deb" "${TMP_DIR}/ashot-gnome-shell-extension.deb"
}

enable_extension() {
  if command -v gnome-extensions >/dev/null 2>&1; then
    gnome-extensions enable "${EXT_UUID}" || {
      echo "Installed ${EXT_UUID}, but enabling it failed. You may need to log out and back in." >&2
      return 0
    }
    return 0
  fi

  echo "Installed ${EXT_UUID}, but gnome-extensions was not found. Enable it manually after logging into GNOME." >&2
}

need_command curl
if [ "${VERSION}" = "latest" ]; then
  resolve_latest_version
fi
if [ "${KIND}" = "auto" ]; then
  KIND="$(detect_package_kind)"
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

case "${KIND}" in
  rpm)
    install_rpm_packages
    ;;
  deb)
    install_deb_packages
    ;;
  *)
    echo "Usage: curl -fsSL <install.sh-url> | sh -s -- [auto|rpm|deb]" >&2
    exit 1
    ;;
esac

enable_extension

echo "Installed aShot ${VERSION} and ${EXT_UUID}."
