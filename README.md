# bohio

[![Version](https://img.shields.io/crates/v/bohio.svg)](https://crates.io/crates/bohio)
[![Rust version](https://img.shields.io/crates/msrv/bohio.svg)](https://crates.io/crates/bohio)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio)](https://docs.rs/bohio)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

Bohío is a remote host runtime. **A proven peer runs commands, reads files and opens shells on a
machine without SSH, WinRM or Python**, over any coqui transport, on physical computers, virtual
machines and containers. It is an application: the `coqui-*` names are reserved for libraries.

## Features

- **Per-peer grants**: who a peer is comes from the trust store; what it may do (`info`,
  `fs.read`, `fs.write`, `exec`, `shell`, `admin`, filesystem roots, a program list) comes from
  `peers.cfg`. A peer with no grant gets nothing.
- **One stream per operation** with a versioned, language-independent protocol: control messages
  are CBOR; bulk bytes travel raw behind a one-byte tag.
- **Threads, not an async runtime**: one thread per stream, bounded by configuration.
- **No hard-coded limits**: every bound is a setting in `bohio.cfg` with a documented default.

Process execution, the filesystem, interactive shells, the authenticated link and the transports (tcp, pipe,
websocket, serial, stdio) are not built yet.

## Installation

The workspace builds from source; nothing is published yet.

```text
git clone https://github.com/Boriken-Dev/bohio
cd bohio
cargo build --release
```

| Crate | Purpose |
| --- | --- |
| `bohio-proto` | the wire types and their CBOR codec; no I/O |
| `bohio` | the host library and the `bohio` command |

## Quick start

```text
cargo run -p bohio -- info --config examples/bohio.cfg
```

prints this host's capabilities. `examples/bohio.cfg` documents every setting with its default.

## Command line

```text
bohio config check --config examples/bohio.cfg
bohio info --config examples/bohio.cfg --format json
```

See the [`bohio` crate](https://github.com/Boriken-Dev/bohio/tree/main/crates/bohio) for the
configuration search order, the exit statuses and the environment variables.

## API overview

| Crate | Where to look |
| --- | --- |
| `bohio-proto` | [`crates/bohio-proto/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio-proto/AGENTS.md) |
| `bohio` | [`crates/bohio/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio/AGENTS.md) |

## Development

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

### Releasing

Nothing is published: the workspace depends on git revisions.

## License

MPL-2.0; see [LICENSE](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE). Contributors sign
[CLA.md](https://github.com/Boriken-Dev/bohio/blob/main/CLA.md).
