# bohio-api

[![Version](https://img.shields.io/crates/v/bohio-api.svg)](https://crates.io/crates/bohio-api)
[![Rust version](https://img.shields.io/crates/msrv/bohio-api.svg)](https://crates.io/crates/bohio-api)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-api)](https://boriken-dev.github.io/bohio/rust/bohio_api/)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

The provider kinds of Bohío, a remote host runtime: **how a node reaches a terminal, a desktop, power, virtual media, programs and files**,
each one a [guatiao](https://github.com/Boriken-Dev/guatiao) kind. The host (`bohio`) and every provider compile against this crate and
nothing else of Bohío's. Documentation: <https://boriken-dev.github.io/bohio/rust/bohio_api/>.

## Features

A node reaches the system it is for through **sources**, each a configured instance of a **provider**, and each capability is one kind:

| Kind | Trait | A source is |
| --- | --- | --- |
| `bohio_terminal` | `TerminalSource` | a console: a pseudo-terminal, a serial line, a BMC's serial-over-LAN |
| `bohio_desktop` | `DesktopSource` | a screen with input: the node's own, a VNC or RDP server, a BMC's KVM |
| `bohio_power` | `PowerSource` | the system's power, through a BMC, a hypervisor or a smart switch |
| `bohio_media` | `MediaSource` | virtual media: an image the system boots or installs from |
| `bohio_exec` | `ExecSource` | programs: the node's own operating system, or a machine reached over SSH |
| `bohio_files` | `FilesSource` | a directory tree reached through directory handles: the node's own file system, or a machine reached over SFTP |

The crate re-exports `guatiao`, so a provider names guatiao only through it and both sides cannot drift to different versions.
`bohio_api::contract` is the suite every provider of a kind passes, and `bohio-api.kinds.json` describes the kinds for a provider written
in another language.

## Installation

```text
cargo add bohio-api --git https://github.com/Boriken-Dev/bohio
```

| Module | Purpose |
| --- | --- |
| `terminal`, `desktop`, `power`, `media`, `exec`, `files` | the six kinds and the objects they open |
| `model` | the request and answer types of the kinds |
| `vocab` | the words a source and a host agree on (what a source `lacks`, signals, error words) |
| `interval` | `Interval`, the type of a provider setting that is a length of time |
| `contract` | the suite every provider of a kind passes |
| `testing` (feature) | in-memory sources for tests |

`crates/bohio-api/AGENTS.md` is the API header: every item with its signature and contract.

## Quick start

Cargo feature `testing` adds `bohio_api::testing`, in-memory sources a test plays the far end of: nothing in it touches a device, a
screen or a network.

## API overview

A provider is one type with the guatiao derives and one `impl` of a kind. Its settings are its own type, checked against its schema
before the provider is built. `describe` says honestly what the source cannot do (`lacks`).

```rust,ignore
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::{SourceInfo, Terminal, TerminalOpen, TerminalSource};

struct Mine;

impl TerminalSource for Mine {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new("mine", &["read", "write"], &["resize", "signals", "programs"]))
    }
    fn open(&self, request: TerminalOpen) -> Result<Object<dyn Terminal>, ProviderError> {
        /* return a value implementing `Terminal`, `.into_object()` */
        # unimplemented!()
    }
}
```

A kind grows by appending methods with a default body; a model grows by fields that default. Each kind's table hash is pinned by
`tests/kinds.rs`, so a change that breaks providers already built is made on purpose.

## Development

```text
cargo fmt --all --check
cargo clippy -p bohio-api --all-targets -- -D warnings
cargo test -p bohio-api
RUSTDOCFLAGS="-D warnings" cargo doc -p bohio-api --no-deps
```

### Releasing

Nothing is published to a registry: the workspace depends on git revisions, and a `v*` tag of the repository builds the release
archives.

## License

MPL-2.0.
