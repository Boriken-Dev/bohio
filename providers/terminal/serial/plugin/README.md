# bohio-provider-serial-plugin

[![Version](https://img.shields.io/crates/v/bohio-provider-serial-plugin.svg)](https://crates.io/crates/bohio-provider-serial-plugin)
[![Rust version](https://img.shields.io/crates/msrv/bohio-provider-serial-plugin.svg)](https://crates.io/crates/bohio-provider-serial-plugin)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-provider-serial-plugin)](https://boriken-dev.github.io/bohio/rust/bohio_provider_serial_plugin/)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

The `serial` terminal provider of [Bohío](https://github.com/Boriken-Dev/bohio) (the console of a system reached through a serial line) **built as a library a node maps at start**: a `cdylib` and nothing else. Documentation: <https://boriken-dev.github.io/bohio/rust/bohio_provider_serial_plugin/>.

## Features

- One library that offers provider `serial`: name the file, or a directory holding it, in `[providers] paths`.
- A node built with the matching Cargo feature already has `serial` built in; this library is for a build without it, and is
  also an example of a provider library.

## Installation

Build the library from a checkout of the repository:

```text
cargo build --release -p bohio-provider-serial-plugin
```

The built library is a `.so`, `.dll` or `.dylib` in the target directory: copy it where the node's `[providers] paths` names.

## Quick start

```toml
[providers]
paths = ["/usr/local/lib/bohio/providers"]   # a file, or a directory of libraries

[source.mine]
provider = "serial"
path = "/dev/ttyUSB0"
```

`bohio providers` lists each library the node loaded, with its digest.

## API overview

None: this crate has no Rust API. It exports guatiao's entry symbol and declares the provider. Its `AGENTS.md` is its header.

## Development

```text
cargo fmt --all --check
cargo clippy -p bohio-provider-serial-plugin --all-targets -- -D warnings
cargo test -p bohio-provider-serial-plugin
RUSTDOCFLAGS="-D warnings" cargo doc -p bohio-provider-serial-plugin --no-deps
```

### Releasing

Nothing is published to a registry: the workspace depends on git revisions, and a `v*` tag of the repository builds the release
archives.

## License

MPL-2.0.
