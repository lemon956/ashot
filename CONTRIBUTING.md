# Contributing to aShot

Thanks for your interest in improving aShot! This document describes how to set
up a development environment and the checks your changes are expected to pass.

## Prerequisites

aShot is a Rust workspace targeting Linux with GTK4/libadwaita. You need:

- A recent stable Rust toolchain (`rustup` recommended), with `clippy` and
  `rustfmt` components.
- System development packages: `gtk4`, `libadwaita-1`, `glib2`, `pkg-config`,
  and `xdg-desktop-portal` at runtime.

On Debian/Ubuntu:

```bash
sudo apt-get install -y libgtk-4-dev libadwaita-1-dev libglib2.0-dev pkg-config libssl-dev
```

On Fedora:

```bash
sudo dnf install gtk4-devel libadwaita-devel glib2-devel pkg-config openssl-devel
```

## Workspace layout

- `ashot-core`: config, annotation model, export renderer, undo/redo history (no GTK).
- `ashot-ipc`: DBus constants, proxy definitions, wire-safe outcomes.
- `ashot-capture`: portal-first system screenshot wrapper.
- `ashot-cli`: the `ashot` command-line client.
- `ashot-app`: DBus service host plus GTK/libadwaita editor/pin/settings windows.

Prefer adding logic to `ashot-core` where it can be unit-tested without a GTK
display, and keep `ashot-app` focused on UI wiring.

## Build and run

```bash
cargo build
./target/debug/ashot-cli gui
```

## Required checks

Before opening a pull request, run the same checks CI enforces:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test -p ashot-core --all-features
cargo test -p ashot-cli --all-features
cargo audit            # cargo install cargo-audit (run once)
```

`cargo fmt --all` applies formatting; `cargo clippy --fix` applies the
mechanical lint suggestions. New behavior in `ashot-core` should come with unit
tests next to the code it covers.

## Pull requests

- Keep changes focused; unrelated refactors belong in separate PRs.
- Use clear, imperative commit messages (e.g. `feat(editor): add WebP export`).
- Update `CHANGELOG.md` under `[Unreleased]` for user-visible changes.
- Describe how you verified the change (commands run, manual GUI steps).
