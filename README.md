# aShot

`aShot` is a Linux screenshot workflow inspired by Flameshot. It uses the system screenshot portal to capture images, then opens a GTK/libadwaita editor window for annotation, export, copy, and pin actions.

## Current Status

This repository contains:

- A Rust workspace with `ashot-core`, `ashot-ipc`, `ashot-capture`, `ashot-cli`, and `ashot-app`
- XDG config handling and default save-path persistence
- DBus contracts for screenshot, settings, editor, and pin-window actions
- A portal-first native capture crate
- A Flameshot-style CLI surface
- A GTK/libadwaita editor shell under the `gtk-ui` feature
- A GNOME 50 Shell extension for reliable pinned-window stacking on Wayland
- Core annotation/export logic for text, line, arrow, brush, rectangle, ellipse, marker, mosaic, blur, counter, filled-box, and OCR tools

## Workspace Layout

- `ashot-core`: config, annotation model, export renderer, undo/redo history
- `ashot-ipc`: DBus constants, proxy definitions, wire-safe outcomes
- `ashot-capture`: portal-first system screenshot wrapper
- `ashot-cli`: `ashot gui`, `ashot full`, `ashot screen`, `ashot launcher`, `ashot config`
- `ashot-app`: DBus service host plus GTK/libadwaita launcher/editor/pin/settings windows
- `gnome-shell/extensions/ashot-pin@io.github.ashot`: background GNOME Shell extension that keeps tagged pin windows above other windows

## Build

Core and CLI crates can be checked with:

```bash
cargo check
cargo test -p ashot-core
cargo test -p ashot-cli
```

For local GUI development, build the workspace from the repository root. This builds both the CLI
and the GTK app service that the CLI launches:

```bash
cargo build
./target/debug/ashot-cli gui
```

This requires system development packages for `gtk4` and `libadwaita-1`.

## Release Package Types

GitHub Releases publish several package types. They install different parts of
aShot:

| Package | Installs | Best for | Pin always-on-top |
| --- | --- | --- | --- |
| `io.github.ashot.App-*.flatpak` | Sandboxed app and CLI | Portable app install | Needs separate host extension install |
| `ashot_*_amd64.deb` / `ashot-*.x86_64.rpm` | Native app and CLI | Debian/Fedora-style native install without GNOME integration | Not by itself |
| `ashot-gnome-shell-extension_*_all.deb` / `ashot-gnome-shell-extension-*.noarch.rpm` | GNOME Shell extension only | Adding reliable pinning to an existing native app install | Yes, after enabling the extension |
| `ashot-gnome_*_all.deb` / `ashot-gnome-*.noarch.rpm` | Meta package depending on app + extension | One-command native GNOME 50 install | Yes, after enabling the extension |
| `ashot-pin@io.github.ashot.shell-extension.zip` | GNOME Shell extension bundle only | Manual extension install or Flatpak users | Yes, after enabling the extension |

Native package examples:

```bash
# Debian/Ubuntu style, app only
sudo apt install ./ashot_0.1.0_amd64.deb

# Debian/Ubuntu style, app + GNOME extension through the meta package
sudo apt install ./ashot_0.1.0_amd64.deb \
  ./ashot-gnome-shell-extension_0.1.0_all.deb \
  ./ashot-gnome_0.1.0_all.deb

# Fedora/RHEL style, app + GNOME extension through the meta package
sudo dnf install ./ashot-0.1.0-1.x86_64.rpm \
  ./ashot-gnome-shell-extension-0.1.0-1.noarch.rpm \
  ./ashot-gnome-0.1.0-1.noarch.rpm
```

GNOME Shell extensions are enabled per user, so native extension packages still
need this user-level step:

```bash
gnome-extensions enable ashot-pin@io.github.ashot
```

## Flatpak

The Flatpak package builds the Rust workspace inside the GNOME SDK and installs
two binaries into the sandbox:

- `ashot-app`: the GTK/DBus application service
- `ashot`: the command-line client

Install the required GNOME runtime once:

```bash
flatpak remote-add --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
flatpak install --user flathub org.gnome.Platform//49 org.gnome.Sdk//49
```

Build and install aShot for the current user:

```bash
./scripts/install-flatpak.sh
./scripts/install-gnome-extension.sh
```

The Flatpak script also applies the bus permission needed for the internal
service process to own `io.github.ashot.Service`. If the current commit is
checked out at an exact Git tag, the generated bundle uses that tag in its
filename, for example `build-flatpak/io.github.ashot.App-v0.1.1.flatpak`.
Otherwise it falls back to the current commit, for example
`build-flatpak/io.github.ashot.App-dev-a1b2c3d.flatpak`.

Or run the Flatpak builder command directly:

```bash
cargo vendor --locked flatpak/vendor | sed 's#directory = ".*flatpak/vendor"#directory = "vendor"#' > flatpak/cargo-config.toml
flatpak-builder --user --install --force-clean --disable-rofiles-fuse --install-deps-from=flathub -y build-flatpak flatpak/io.github.ashot.App.json
```

Use the CLI from the installed Flatpak directly:

```bash
flatpak run io.github.ashot.App gui
flatpak run io.github.ashot.App full --delay 500
flatpak run io.github.ashot.App config
```

`flatpak run io.github.ashot.App` without a subcommand also opens the GUI.

For a shorter shell command, add an alias:

```bash
alias ashot='flatpak run io.github.ashot.App'
```

After that, the normal commands work:

```bash
ashot gui
ashot full --path ~/Pictures/Screenshots
ashot config
```

## GNOME 50 Pin Windows

GNOME Wayland does not let a normal GTK application force a global always-on-top
state by itself. aShot uses a small GNOME Shell extension for reliable pinning:

1. aShot starts pin windows through `gnome-service-client -t ashot-pin`.
2. The extension matches windows with the `ashot-pin` tag.
3. The extension calls Mutter's `Meta.Window.make_above()` and keeps the window
   on all workspaces.

Install and enable the user-level extension after installing the app:

```bash
./scripts/install-gnome-extension.sh
```

Flatpak remains the app packaging path, but the GNOME Shell extension is a
host-side component and is not installed inside the Flatpak sandbox. For
distribution packaging, keep the components split:

- `ashot`: the Rust/GTK application and CLI
- `ashot-gnome-shell-extension`: the GNOME Shell extension
- `ashot-gnome`: a meta package that depends on both for one-command installs

## OCR

The editor includes an OCR region tool. By default it uses the local `tesseract`
command and does not upload screenshots. Open `ashot config`, search for the OCR
language you need, and copy the suggested Tesseract language package or full
install command for your Linux distribution.

OCR.space can be enabled as an optional online backend in settings. That backend
uploads the selected OCR region and requires an API key.

## CLI

```bash
ashot gui
ashot gui --path ~/Pictures/Screenshots --clipboard --pin
ashot full --delay 500 --path ~/Pictures/Screenshots
ashot screen --raw > screenshot.png
ashot launcher
ashot config
```

Recommended Linux shortcut command:

```bash
ashot gui
```

## Design Notes

- `ashot gui` uses the system screenshot portal for region selection and opens the GTK editor with the captured image.
- `ashot full` and `ashot screen` use the same portal-first backend for fullscreen capture.
- Reliable GNOME 50 Wayland pin-window stacking uses the `ashot-pin` GNOME Shell extension.
- Upload and tray behavior are intentionally out of scope for this migration pass.
