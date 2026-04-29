# Release Checklist

## Build

- `cargo check`
- `cargo test -p ashot-core`
- `cargo test -p ashot-cli`
- `cargo test -p ashot-capture`
- `cargo check -p ashot-app --features gtk-ui`

## Linux / Wayland Validation

- `ashot gui` launches the portal area picker and opens the GTK editor
- `ashot full` captures fullscreen through the portal
- `ashot screen` captures fullscreen through the portal until per-screen support is added
- Cancelling capture does not crash the service
- Repeated capture requests while one is active return `busy`

## Editor Validation

- Text, line, arrow, brush, rectangle, ellipse, marker, mosaic, blur, counter, and filled-box tools draw and export
- Undo and redo operate on annotation snapshots
- Saving writes into the configured default directory
- Copy action places the rendered image in the clipboard
- Pin action opens a separate preview window

## Packaging

- Flatpak manifest builds successfully
- Flatpak permissions include `--socket=wayland` and do not request X11 or `fallback-x11`
- Desktop file installs under the correct app id
- AppStream metadata validates
- Icon is present
- CLI is exposed for Linux shortcut binding

## Environment Caveats

- Confirm that no GNOME Shell extension is required
- Confirm that the screenshot and editor workflows are validated on a Wayland session
