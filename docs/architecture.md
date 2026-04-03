# aShot Architecture

## Summary

`aShot` is split into small crates so the GNOME/Wayland-sensitive pieces stay isolated from the portable core logic.

## Crates

### `ashot-core`

- Owns `AppConfig`
- Owns the annotation/document model
- Owns undo/redo snapshots
- Owns final PNG rendering and mosaic application
- Avoids GTK-specific dependencies so rendering and tests stay fast

### `ashot-ipc`

- Defines DBus constants:
  - service name `io.github.ashot.App`
  - object path `/io/github/ashot/App`
- Defines serializable operation outcomes
- Generates the client proxy used by `ashot-cli`

### `ashot-capture`

- Wraps the screenshot portal using `ashpd`
- Maps product capture modes to portal request settings
- Returns a file URI that the editor and pin window can consume

### `ashot-cli`

- Parses capture and helper commands
- Connects to the DBus service
- Best-effort launches `ashot-app --service` when the service is not yet running
- Returns meaningful exit codes for cancelled/busy/unsupported/failure states

### `ashot-app`

- Registers the DBus service
- Serializes capture requests with a mutex to prevent concurrent sessions
- Opens launcher, settings, editor, and pin windows on the GTK side
- Keeps GTK-specific code behind the `gtk-ui` feature because the build requires system GTK/libadwaita development packages

## Data Flow

1. `ashot capture area` runs in the CLI.
2. The CLI connects to the DBus service and requests a capture.
3. `ashot-app` uses `ashot-capture` to call the screenshot portal.
4. The portal returns a file URI.
5. `ashot-app` optionally opens the editor with that image.
6. The editor mutates an `ashot-core::Document`.
7. Saving or pinning renders the document back into a PNG via `ashot-core`.

## Editor Model

- Base image stays immutable in memory
- Overlay annotations are tracked separately
- Undo/redo stores lightweight snapshots of the annotation list
- Export is a rasterization pass over the immutable base image plus overlays

## Known Gaps

- The GTK feature could not be compile-validated in this environment because the machine does not provide `gtk4` and `libadwaita-1` development files.
- The current text tool uses a blocking prompt dialog and should later move to a better inline editing affordance.
- Tray/AppIndicator support is intentionally not hard-required and is not yet implemented in this first repository pass.
