# bohio-client

[![Version](https://img.shields.io/crates/v/bohio-client.svg)](https://crates.io/crates/bohio-client)
[![Rust version](https://img.shields.io/crates/msrv/bohio-client.svg)](https://crates.io/crates/bohio-client)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-client)](https://docs.rs/bohio-client)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

A Rust client for Bohío hosts, over any coqui channel. **It owns no link**: a caller implements
`Open` (one stream per operation, whose open carries the request) and gets typed operations back,
so a theater provider, a command line and the host's own tests share one client. Documentation:
<https://docs.rs/bohio-client>.

## Features

- `exec::Exec`: run one process, stream its output, write its input, kill it, wait for its exit.
- `fs::Fs`, `fs::Reader`, `fs::Writer`: stat, list, create, remove, rename, copy and checksum files on the host, and
  stream a file in either direction in chunks.
- `shell::Shell`: an interactive shell on a terminal: type, read output, resize, kill.
- `control::Control`: the control stream the small calls share.
- Blocking calls with explicit timeouts; every method takes `&self`, so reading and writing can
  run on different threads.
- Errors keep the host's structured `{code, message}` apart from channel failures.

## Installation

```text
cargo add bohio-client --git https://github.com/Boriken-Dev/bohio
```

The crate has no Cargo features.

## Quick start

A client needs an `Open` for its link; `exec` is then three lines:

```rust
use std::time::Duration;

use bohio_client::exec::Exec;
use bohio_client::{Error, Open};
use bohio_proto::exec::Request;

fn run_echo(link: &dyn Open) -> Result<Vec<u8>, Error> {
    let exec = Exec::start(link, &Request::argv(["echo", "hello"]))?;
    let done = exec.wait(Some(Duration::from_secs(30)))?;
    Ok(done.stdout)
}
# fn main() {}
```

## API overview

| Item | Purpose |
| --- | --- |
| `Open` | opens one stream for a protocol; implemented over a link |
| `Error` | a host error, a channel failure, a timeout or a closed stream |
| `exec` | `Exec`, `Event` and `Completed` for `bohio-exec/1` |

## Development

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

### Releasing

Nothing is published: the workspace depends on git revisions.

## License

MPL-2.0; see [LICENSE](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE).
