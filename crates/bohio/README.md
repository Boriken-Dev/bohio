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

- Process execution: a program and its arguments run directly, never through a shell unless the
  caller names one; output streams as it is written; a deadline, a kill request or a closed
  stream ends the whole process tree.
- Filesystem access confined to the peer's granted roots: a `..` or a symbolic link that leaves a root is refused,
  every path component is resolved against a directory handle, and files stream in chunks with atomic replace.
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

Grant a peer `exec` and serve one request over an in-process channel:

```rust
use std::sync::Arc;

use bohio::config::ExecutionConfig;
use bohio::exec::{self, Context};
use bohio::limit::Slots;
use bohio::{Grant, Permission};
use bohio_proto::exec::Request;
use coqui_transport::frame::LengthPrefix;
use coqui_transport::{memory, StreamChannel};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let (host_end, _client_end) = memory::pipe(64 * 1024);
let channel = Arc::new(StreamChannel::start(host_end, LengthPrefix::u32_be())?);

let grant = Grant::allowing([Permission::Info]);
let config = ExecutionConfig::default();
let slots = Slots::new(config.max_processes);
let context = Context { grant: &grant, config: &config, slots: &slots };

// This peer lacks `exec`, so the request is refused.
let metadata = Request::argv(["echo", "hi"]).to_metadata()?;
assert!(exec::serve(channel, &metadata, &context).is_err());
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
| `exec` | serves one `bohio-exec/1` stream |
| `info` | this host's capabilities |
| `limit` | a counted bound on concurrent work |
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
