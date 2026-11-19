# bohio

[![Version](https://img.shields.io/crates/v/bohio.svg)](https://crates.io/crates/bohio)
[![Rust version](https://img.shields.io/crates/msrv/bohio.svg)](https://crates.io/crates/bohio)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio)](https://docs.rs/bohio)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

Bohío is a remote host runtime: **an authenticated peer runs commands, reads files and opens
shells on a machine without SSH, WinRM or Python**, over any coqui transport. This crate is the
host: a library of engines, each serving one operation over a channel, and the `bohio` command.
Documentation: <https://docs.rs/bohio>.

## Features

- Per-peer grants (`peers.cfg`): an approved peer with no `allow` gets nothing.
- Threads, not an async runtime: one thread per stream, bounded by configuration.
- Every bound is a setting with a documented default; nothing is hard-coded.

## Installation

```text
cargo add bohio --git https://github.com/Boriken-Dev/bohio
```

| Feature | Adds | Default |
| --- | --- | --- |
| `cli` | the `bohio` command and the `bohio::cli` module | on |

An embedding host takes the library alone with `default-features = false`.

## Quick start

Read a peer grant and a configuration:

```rust
use bohio::policy::{Permission, Policies};
use bohio::Config;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = Config::from_toml("[execution]
max_processes = 4
", "bohio.cfg")?;
assert_eq!(config.execution.max_processes, 4);

let policies = Policies::from_toml("[peer.laptop]
allow = [\"info\", \"exec\"]
", "peers.cfg")?;
assert!(policies.grant_for("laptop").allows(Permission::Exec));
assert!(!policies.grant_for("stranger").allows(Permission::Info));
# Ok(())
# }
```

## Command line

Run from the repository root:

```text
bohio config check --config examples/bohio.cfg
bohio info --config examples/bohio.cfg
bohio info --config examples/bohio.cfg --format json
```

`bohio config check` reads the configuration and prints `ok` with a summary, or one line naming
the file, the position and the problem. `bohio info` prints this host's capabilities. With no
`--config`, the first file that exists of `$BOHIO_CONFIG`, the user's `bohio.cfg` and the
system's is used, and with none the defaults apply.

| Status | Meaning |
| --- | --- |
| 0 | success |
| 1 | the operation failed |
| 2 | the invocation was wrong |

## API overview

| Module | Purpose |
| --- | --- |
| `config` | `bohio.cfg`: `Config`, its sections and the `Environment` searches read |
| `policy` | `peers.cfg`: `Policies`, `Grant` and `Permission` |
| `info` | this host's capabilities |
| `cli` | the command line (feature `cli`) |

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

MPL-2.0; see [LICENSE](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE).
