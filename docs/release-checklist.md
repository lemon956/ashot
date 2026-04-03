# Release Checklist

## Build

- `cargo check`
- `cargo test -p ashot-core`
- `cargo run -p ashot-app --features gtk-ui`

## GNOME / Wayland Validation

- Region capture works through the portal
- Screen capture works through the portal
- Window capture launches the interactive picker path
- Cancelling capture does not crash the service
- Repeated capture requests while one is active return `busy`

## Editor Validation

- Text annotation inserts and exports
- Arrow annotation draws and exports
- Brush annotation draws and exports
- Rectangle annotation draws and exports
- Mosaic annotation pixelates and exports
- Undo and redo operate on annotation snapshots
- Saving writes into the configured default directory
- Copy action places the rendered image in the clipboard
- Pin action opens a separate preview window

## Packaging

- Flatpak manifest builds successfully
- Desktop file installs under the correct app id
- AppStream metadata validates
- Icon is present
- CLI is exposed for GNOME shortcut binding

## Environment Caveats

- Validate both “no tray extension installed” and “tray extension installed” environments
- Confirm that no X11-only dependencies are required for the screenshot path
