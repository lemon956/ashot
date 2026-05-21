#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${OUTPUT_DIR:-${ROOT_DIR}/build-packages}"
WORK_DIR="${OUTPUT_DIR}/work"
EXT_UUID="ashot-pin@io.github.ashot"
EXT_SOURCE="${ROOT_DIR}/gnome-shell/extensions/${EXT_UUID}"
BINARY_DIR="${ASHOT_BINARY_DIR:-${ROOT_DIR}/target/release}"
APP_BIN="${BINARY_DIR}/ashot-app"
CLI_BIN="${BINARY_DIR}/ashot-cli"

VERSION="${ASHOT_VERSION:-$(git -C "${ROOT_DIR}" describe --tags --exact-match 2>/dev/null || true)}"
if [ -z "${VERSION}" ]; then
  SHORT_COMMIT="$(git -C "${ROOT_DIR}" rev-parse --short=7 HEAD 2>/dev/null || true)"
  VERSION="0.0.0+dev.${SHORT_COMMIT:-unknown}"
fi
PACKAGE_VERSION="${VERSION#v}"
RPM_VERSION="${PACKAGE_VERSION//-/_}"
RPM_VERSION="${RPM_VERSION//+/_}"

if [ ! -x "${APP_BIN}" ] || [ ! -x "${CLI_BIN}" ]; then
  echo "Release binaries not found. Run cargo build --release -p ashot-app -p ashot-cli first." >&2
  exit 1
fi

if [ ! -d "${EXT_SOURCE}" ]; then
  echo "GNOME Shell extension source directory not found: ${EXT_SOURCE}" >&2
  exit 1
fi

if ! command -v dpkg-deb >/dev/null 2>&1; then
  echo "dpkg-deb is required to build Debian packages." >&2
  exit 1
fi

if ! command -v rpmbuild >/dev/null 2>&1; then
  echo "rpmbuild is required to build RPM packages." >&2
  exit 1
fi

rm -rf "${WORK_DIR}"
mkdir -p "${OUTPUT_DIR}" "${WORK_DIR}"

install_app_payload() {
  local root="$1"

  install -Dm755 "${APP_BIN}" "${root}/usr/bin/ashot-app"
  install -Dm755 "${CLI_BIN}" "${root}/usr/bin/ashot"
  install -Dm644 "${ROOT_DIR}/packaging/io.github.ashot.App.desktop" \
    "${root}/usr/share/applications/io.github.ashot.App.desktop"
  install -Dm644 "${ROOT_DIR}/packaging/io.github.ashot.App.metainfo.xml" \
    "${root}/usr/share/metainfo/io.github.ashot.App.metainfo.xml"
  install -Dm644 "${ROOT_DIR}/packaging/io.github.ashot.App.svg" \
    "${root}/usr/share/icons/hicolor/scalable/apps/io.github.ashot.App.svg"
}

install_extension_payload() {
  local root="$1"

  install -d "${root}/usr/share/gnome-shell/extensions"
  cp -R "${EXT_SOURCE}" "${root}/usr/share/gnome-shell/extensions/${EXT_UUID}"
}

write_deb_control() {
  local package="$1"
  local arch="$2"
  local depends="$3"
  local description="$4"
  local root="$5"

  install -d "${root}/DEBIAN"
  cat >"${root}/DEBIAN/control" <<EOF
Package: ${package}
Version: ${PACKAGE_VERSION}
Section: graphics
Priority: optional
Architecture: ${arch}
Maintainer: aShot Maintainers <maintainers@example.invalid>
Depends: ${depends}
Description: ${description}
EOF
}

build_deb_package() {
  local package="$1"
  local arch="$2"
  local depends="$3"
  local description="$4"
  local root="${WORK_DIR}/deb/${package}"

  mkdir -p "${root}"
  write_deb_control "${package}" "${arch}" "${depends}" "${description}" "${root}"
  case "${package}" in
    ashot)
      install_app_payload "${root}"
      ;;
    ashot-gnome-shell-extension)
      install_extension_payload "${root}"
      ;;
    ashot-gnome)
      ;;
  esac
  dpkg-deb --root-owner-group --build "${root}" \
    "${OUTPUT_DIR}/${package}_${PACKAGE_VERSION}_${arch}.deb"
}

write_rpm_spec() {
  local package="$1"
  local arch="$2"
  local requires="$3"
  local description="$4"
  local payload_root="$5"
  local spec_path="$6"

  cat >"${spec_path}" <<EOF
Name: ${package}
Version: ${RPM_VERSION}
Release: 1%{?dist}
Summary: ${description}
License: MIT
EOF
  if [ "${arch}" = "noarch" ]; then
    echo "BuildArch: noarch" >>"${spec_path}"
  fi
  if [ -n "${requires}" ]; then
    echo "Requires: ${requires}" >>"${spec_path}"
  fi
  cat >>"${spec_path}" <<EOF

%description
${description}

%install
mkdir -p %{buildroot}
cp -a ${payload_root}/usr %{buildroot}/

%files
/usr
EOF
}

build_rpm_package() {
  local package="$1"
  local arch="$2"
  local requires="$3"
  local description="$4"
  local payload_root="${WORK_DIR}/rpm-payload/${package}"
  local rpm_top="${WORK_DIR}/rpmbuild-${package}"
  local spec_path="${rpm_top}/SPECS/${package}.spec"

  mkdir -p "${payload_root}" "${rpm_top}/SPECS" "${rpm_top}/BUILD" "${rpm_top}/RPMS" \
    "${rpm_top}/SRPMS" "${rpm_top}/SOURCES"
  case "${package}" in
    ashot)
      install_app_payload "${payload_root}"
      ;;
    ashot-gnome-shell-extension)
      install_extension_payload "${payload_root}"
      ;;
    ashot-gnome)
      install -d "${payload_root}/usr/share/doc/ashot-gnome"
      echo "Meta package for aShot GNOME integration." \
        >"${payload_root}/usr/share/doc/ashot-gnome/README"
      ;;
  esac
  write_rpm_spec "${package}" "${arch}" "${requires}" "${description}" \
    "${payload_root}" "${spec_path}"
  rpmbuild --define "_topdir ${rpm_top}" -bb "${spec_path}"
  find "${rpm_top}/RPMS" -type f -name "*.rpm" -exec cp {} "${OUTPUT_DIR}/" \;
}

DEB_APP_DEPS="libc6, libgtk-4-1, libadwaita-1-0, libglib2.0-0, xdg-desktop-portal"
DEB_EXT_DEPS="gnome-shell (>= 50)"
RPM_APP_DEPS="gtk4, libadwaita, glib2, xdg-desktop-portal"
RPM_EXT_DEPS="gnome-shell >= 50"

build_deb_package "ashot" "amd64" "${DEB_APP_DEPS}" \
  "Wayland-native screenshot workflow for GNOME"
build_deb_package "ashot-gnome-shell-extension" "all" "${DEB_EXT_DEPS}" \
  "GNOME Shell extension for aShot pinned windows"
build_deb_package "ashot-gnome" "all" \
  "ashot (= ${PACKAGE_VERSION}), ashot-gnome-shell-extension (= ${PACKAGE_VERSION})" \
  "Meta package for aShot with GNOME pin-window integration"

build_rpm_package "ashot" "x86_64" "${RPM_APP_DEPS}" \
  "Wayland-native screenshot workflow for GNOME"
build_rpm_package "ashot-gnome-shell-extension" "noarch" "${RPM_EXT_DEPS}" \
  "GNOME Shell extension for aShot pinned windows"
build_rpm_package "ashot-gnome" "noarch" \
  "ashot = ${RPM_VERSION}, ashot-gnome-shell-extension = ${RPM_VERSION}" \
  "Meta package for aShot with GNOME pin-window integration"

echo "Built packages in ${OUTPUT_DIR}:"
find "${OUTPUT_DIR}" -maxdepth 1 -type f \( -name "*.deb" -o -name "*.rpm" \) -print | sort
