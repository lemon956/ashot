# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- The editor canvas now shows the real export result for every tool: committed
  annotations are painted from the rendered document, so Mosaic, Blur, Counter,
  and Marker look exactly like the saved image (WYSIWYG).
- Mosaic and Blur draft previews now render the real effect while you drag,
  instead of a placeholder pattern.
- Pin-window zooming is throttled so continuous scroll no longer stutters
  (the resize/relayout is coalesced and the window is re-asserted on top only
  after zooming settles).

## [0.2.8]

### Fixed

- Avoid a potential integer division-by-zero in the mosaic/pixelate region.

### Changed

- Pin the CI Rust toolchain so the `cargo clippy -D warnings` and `cargo fmt`
  gate is deterministic; toolchain upgrades are now deliberate instead of
  breaking CI on every new stable release.
- Dependabot ignores semver-major cargo updates: the GTK4/GLib stack is pinned
  and breaks when those crates are bumped in isolation.

## [0.2.7]

### Added

- Export screenshots as PNG, JPEG, or WebP, with a format picker in the save
  dialog and a configurable default format and JPEG quality.
- The color picker copies the sampled color's hex to the clipboard, and OCR
  copies the recognized text automatically.
- Drag on empty canvas to pan the viewport while the Select tool is active.
- Project docs: `CHANGELOG.md`, `CONTRIBUTING.md`, `SECURITY.md`, and a
  Dependabot configuration.

### Changed

- Saving no longer overwrites an existing file: a ` (n)` suffix is added when
  the target name is already taken.
- Faster blur: the region blur uses a separable, prefix-sum box blur
  (independent of radius) and no longer clones the whole image for blur/mosaic.
- CI now enforces `cargo fmt --check`, `cargo clippy -D warnings`, and a
  `cargo audit` dependency security pass.

## [0.2.6]

### Added

- High-quality zoom cache, unified zoom controls, and editor keyboard shortcuts.

## [0.2.5]

### Added

- One-line release installer for native RPM/DEB packages from GitHub Releases.

## [0.2.4]

### Added

- GNOME Shell pin extension and distribution packaging (RPM/DEB/Flatpak).

## [0.1.0]

### Added

- Rust workspace with `ashot-core`, `ashot-ipc`, `ashot-capture`, `ashot-cli`,
  and `ashot-app`.
- Portal-first native screenshot capture (area, screen, window).
- GTK/libadwaita editor with text, line, arrow, brush, rectangle, ellipse,
  marker, mosaic, blur, counter, filled-box, color-picker, and OCR tools.
- Undo/redo history, clipboard copy, save-to-file, and pin-on-top actions.
- XDG config handling with a graphical preferences window.
- Flameshot-style CLI surface (`gui`, `full`, `screen`, `launcher`, `config`).
- OCR via local Tesseract with an optional OCR.space online backend.

[Unreleased]: https://github.com/lemon956/ashot/compare/v0.2.8...HEAD
[0.2.8]: https://github.com/lemon956/ashot/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/lemon956/ashot/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/lemon956/ashot/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/lemon956/ashot/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/lemon956/ashot/compare/v0.1.0...v0.2.4
[0.1.0]: https://github.com/lemon956/ashot/releases/tag/v0.1.0
