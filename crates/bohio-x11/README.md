# bohio-x11

[![Version](https://img.shields.io/crates/v/bohio-x11.svg)](https://crates.io/crates/bohio-x11)
[![Rust version](https://img.shields.io/crates/msrv/bohio-x11.svg)](https://crates.io/crates/bohio-x11)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-x11)](https://boriken-dev.github.io/bohio/rust/bohio_x11/)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

The desktop a Bohío host serves, and **the X11 backend that serves an X display** over any stream. Documentation: <https://boriken-dev.github.io/bohio/rust/bohio_x11/>.

## Features

- `Desktop`, `DesktopState`, `Frame`, `GuiError`, `Capabilities`: what a desktop backend implements and the GUI engine drives.
- `X11Desktop` over `X11Settings`: the screen of an X display (RandR monitors, `GetImage` capture, XTest input, a clipboard
  selection), in pure safe Rust over `x11rb`. It connects by display name, or over the stream a `Connector` hands it, so the
  display of another machine (an SSH channel) is served by the same code.
- `source::DesktopOf`: any `Desktop` as a `bohio_desktop` source of `bohio-api`.

## Installation

```text
cargo add bohio-x11 --git https://github.com/Boriken-Dev/bohio
```

| Module | Purpose |
| --- | --- |
| `desktop` | the `Desktop` trait and its state, frame and error types |
| `x11` | `X11Desktop`, `X11Settings` and the `Connector` of a stream |
| `source` | `DesktopOf`: a `Desktop` as a `bohio_desktop` source |
| `logind` (feature) | lock detection |

`crates/bohio-x11/AGENTS.md` is the API header.

## Quick start

Cargo feature `logind` (Linux) adds lock detection from logind's `LockedHint` on the system bus; without it a desktop cannot tell a
locked screen. `bohio` re-exports the types as `bohio::gui::*`.

## API overview

The backend is driven by the `bohio` host, which serves it to peers that hold the desktop permissions: see `bohio serve` and
`bohio remote ... gui` in the [`bohio` crate](https://github.com/Boriken-Dev/bohio/tree/main/crates/bohio). To serve an X display by
itself, build an `X11Desktop` from `X11Settings` and wrap it with `source::DesktopOf` to register it as a source.

## Development

```text
cargo fmt --all --check
cargo clippy -p bohio-x11 --all-targets -- -D warnings
cargo test -p bohio-x11
RUSTDOCFLAGS="-D warnings" cargo doc -p bohio-x11 --no-deps
```

### Releasing

Nothing is published to a registry: the workspace depends on git revisions, and a `v*` tag of the repository builds the release
archives.

## License

MPL-2.0.
