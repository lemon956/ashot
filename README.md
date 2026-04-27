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
- Core annotation/export logic for text, line, arrow, brush, rectangle, ellipse, marker, mosaic, blur, counter, filled-box, and OCR tools

## Workspace Layout

- `ashot-core`: config, annotation model, export renderer, undo/redo history
- `ashot-ipc`: DBus constants, proxy definitions, wire-safe outcomes
- `ashot-capture`: portal-first system screenshot wrapper
- `ashot-cli`: `ashot gui`, `ashot full`, `ashot screen`, `ashot launcher`, `ashot config`
- `ashot-app`: DBus service host plus GTK/libadwaita launcher/editor/pin/settings windows

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

## Flatpak

The recommended local installation path is Flatpak. The manifest builds the Rust
workspace inside the GNOME SDK and installs two binaries into the sandbox:

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
```

This script also applies the Flatpak bus permission needed for the internal
service process to own `io.github.ashot.Service`.

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
- GNOME Shell extension integration has been removed; the app no longer depends on Shell-side overlay code.
- Upload and tray behavior are intentionally out of scope for this migration pass.
