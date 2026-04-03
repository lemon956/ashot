# aShot

`aShot` is a GNOME-first screenshot workflow for Wayland. The project now has two layers:

- a Rust app for config, export, saving, pinning, and DBus integration
- a GNOME Shell extension for the Flameshot-like in-place capture overlay

## Current Status

This repository now contains:

- A Rust workspace with `ashot-core`, `ashot-ipc`, `ashot-capture`, `ashot-cli`, and `ashot-app`
- A GNOME Shell extension under `gnome-shell-extension/ashot-shell@io.github.ashot`
- XDG config handling and default save-path persistence
- DBus contracts for screenshot, settings, editor, pin-window actions, and shell-overlay start/finalize flow
- A portal-backed capture crate using `ashpd`
- A CLI entrypoint suitable for GNOME custom keyboard shortcuts
- A GTK/libadwaita app shell under the `gtk-ui` feature
- Core annotation/export logic for text, arrows, brush, rectangles, and mosaic

## Workspace Layout

- `ashot-core`: config, annotation model, export renderer, undo/redo history
- `ashot-ipc`: DBus constants, proxy definitions, wire-safe outcomes
- `ashot-capture`: screenshot portal wrapper
- `ashot-cli`: `ashot capture ...`, `ashot open-settings`, `ashot pin ...`
- `ashot-app`: DBus service host plus GTK/libadwaita launcher/editor/pin/settings windows
- `gnome-shell-extension/ashot-shell@io.github.ashot`: fullscreen overlay, region selection, inline toolbar, and save handoff back to Rust

## Build

Core and CLI crates can be checked with:

```bash
cargo check
cargo test -p ashot-core
```

The GTK shell is implemented behind a feature flag:

```bash
cargo run -p ashot-app --features gtk-ui
```

This requires system development packages for `gtk4` and `libadwaita-1`.

## Shell Extension

The new interactive area workflow lives in the GNOME Shell extension. Install it locally with:

```bash
./scripts/install-shell-extension.sh
gnome-extensions enable ashot-shell@io.github.ashot
```

Once enabled, you can:

- click the `aShot` panel icon and choose `Area Capture`
- or run `ashot capture area`

The `capture area` command now expects the extension to be enabled. It does not silently fall back to the old portal picker.

## CLI

```bash
ashot capture area
ashot capture screen
ashot capture window
ashot open-settings
ashot pin /absolute/path/to/image.png
```

Recommended GNOME shortcut command:

```bash
ashot capture area
```

## Design Notes

- `capture area` is now intended to go through the Shell extension so the user can keep working inside the live screen context.
- `capture screen` and `capture window` still go through the Rust service and screenshot backend directly.
- The old separate GTK editor remains in-tree, but it is no longer the intended primary interaction for region capture.
