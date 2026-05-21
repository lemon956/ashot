# aShot Architecture

## Summary

`aShot` is split into small Rust crates so system screenshot capture, DBus integration, GTK UI, and portable annotation/export logic remain separated. The capture/editor path is portal-first; reliable GNOME 50 Wayland pin stacking is handled by a small GNOME Shell extension.

## Crates

### `ashot-core`

- Owns `AppConfig`
- Owns the annotation/document model
- Owns undo/redo snapshots
- Owns final PNG rendering for Flameshot-style local editing tools
- Avoids GTK-specific dependencies so rendering and tests stay fast

### `ashot-ipc`

- Defines DBus constants:
  - service name `io.github.ashot.Service`
  - object path `/io/github/ashot/App`
  - interface `io.github.ashot.App`
- Defines serializable operation outcomes
- Generates the client proxy used by `ashot-cli`

### `ashot-capture`

- Wraps the system screenshot portal
- Uses interactive portal mode for area/window capture
- Uses non-interactive portal mode for fullscreen capture
- Returns a file URI that the editor and pin window can consume

### `ashot-cli`

- Parses Flameshot-style commands: `gui`, `full`, `screen`, `launcher`, and `config`
- Connects to the DBus service
- Best-effort launches `ashot-app --service` when the service is not yet running
- Returns meaningful exit codes for cancelled/busy/unsupported/failure states

### `ashot-app`

- Registers the DBus service
- Serializes capture requests with a mutex to prevent concurrent sessions
- Opens launcher, settings, editor, and pin windows on the GTK side
- Starts pin windows through `gnome-service-client -t ashot-pin` so only pin viewer processes receive the window tag
- Keeps GTK-specific code behind the `gtk-ui` feature because the build requires system GTK/libadwaita development packages

### GNOME Shell Extension

- Lives under `gnome-shell/extensions/ashot-pin@io.github.ashot`
- Runs in the background without a top-panel indicator
- Matches pin windows by the `ashot-pin` tag
- Calls `Meta.Window.make_above()` and `stick()` so GNOME Shell/Mutter, not GTK, owns the global stacking behavior

## Data Flow

1. `ashot gui` runs in the CLI.
2. The CLI ensures the DBus service is available.
3. `ashot-app` calls `ashot-capture`.
4. `ashot-capture` asks the system screenshot portal for an interactive area capture.
5. The portal returns a file URI.
6. `ashot-app` opens the GTK editor with that image.
7. The editor mutates an `ashot-core::Document`.
8. Saving, copying, or pinning renders the document back into a PNG via `ashot-core`.
9. Pinning launches a tagged pin viewer; the GNOME Shell extension keeps that tagged window above normal application windows.

## Editor Model

- Base image stays immutable in memory
- Overlay annotations are tracked separately
- Undo/redo stores lightweight snapshots of the annotation list
- Export is a rasterization pass over the immutable base image plus overlays
- Supported local tools include text, line, arrow, brush, rectangle, ellipse, marker, mosaic, blur, counter, and filled boxes

## Known Gaps

- `--region`, `--last-region`, and geometry printing depend on desktop support that is not yet implemented.
- Upload and tray behavior are intentionally not implemented in this migration pass.
- The current text tool uses a blocking prompt dialog and should later move to a better inline editing affordance.
