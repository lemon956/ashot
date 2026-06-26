# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Export screenshots as PNG, JPEG, or WebP, with a format picker in the save
  dialog and a configurable default format and JPEG quality.
- The color picker copies the sampled color's hex to the clipboard, and OCR
  copies the recognized text automatically.
- One-line release installer for native RPM/DEB packages from GitHub Releases.
- High-quality zoom cache, unified zoom controls, and keyboard shortcuts in the editor.
- Drag on empty canvas to pan the viewport while the Select tool is active.
- GNOME Shell pin extension and distribution packaging (RPM/DEB/Flatpak).

### Changed

- Saving no longer overwrites an existing file: a ` (n)` suffix is added when the
  target name is taken.
- Faster blur: the region blur now uses a separable, prefix-sum box blur
  (independent of radius) and no longer clones the whole image for blur/mosaic.
- CI now enforces `cargo fmt --check`, `cargo clippy -D warnings`, and a
  `cargo audit` dependency security pass.

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

[Unreleased]: https://github.com/lemon956/ashot/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/lemon956/ashot/releases/tag/v0.1.0
