# bohio-provider-ssh-plugin

[![Version](https://img.shields.io/crates/v/bohio-provider-ssh-plugin.svg)](https://crates.io/crates/bohio-provider-ssh-plugin)
[![Rust version](https://img.shields.io/crates/msrv/bohio-provider-ssh-plugin.svg)](https://crates.io/crates/bohio-provider-ssh-plugin)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-provider-ssh-plugin)](https://boriken-dev.github.io/bohio/rust/bohio_provider_ssh_plugin/)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

The `ssh` provider of [Bohío](https://github.com/Boriken-Dev/bohio) (a machine reached over SSH) **built as a library a node maps at start**: a `cdylib` and nothing else. Documentation: <https://boriken-dev.github.io/bohio/rust/bohio_provider_ssh_plugin/>.

## Features

- One library that offers provider `ssh`: name the file, or a directory holding it, in `[providers] paths`.
- A node built with the matching Cargo feature already has `ssh` built in; this library is for a build without it, and is
  also an example of a provider library.

## Installation

Build the library from a checkout of the repository:

```text
cargo build --release -p bohio-provider-ssh-plugin
```

The built library is a `.so`, `.dll` or `.dylib` in the target directory: copy it where the node's `[providers] paths` names.

## Quick start

```toml
[providers]
paths = ["/usr/local/lib/bohio/providers"]   # a file, or a directory of libraries

[source.mine]
provider = "ssh"
host = "build.example"
```

`bohio providers` lists each library the node loaded, with its digest.

## API overview

None: this crate has no Rust API. It exports guatiao's entry symbol and declares the provider. Its `AGENTS.md` is its header.

## Development

```text
cargo fmt --all --check
cargo clippy -p bohio-provider-ssh-plugin --all-targets -- -D warnings
cargo test -p bohio-provider-ssh-plugin
RUSTDOCFLAGS="-D warnings" cargo doc -p bohio-provider-ssh-plugin --no-deps
```

### Releasing

Nothing is published to a registry: the workspace depends on git revisions, and a `v*` tag of the repository builds the release
archives.

## License

MPL-2.0.
