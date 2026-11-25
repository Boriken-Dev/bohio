# bohio-client

[![Version](https://img.shields.io/crates/v/bohio-client.svg)](https://crates.io/crates/bohio-client)
[![Rust version](https://img.shields.io/crates/msrv/bohio-client.svg)](https://crates.io/crates/bohio-client)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-client)](https://docs.rs/bohio-client)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

A Rust client for Bohío hosts, over any coqui channel. **The operations own no link**: a caller implements
`Open` (one stream per operation, whose open carries the request) and gets typed operations back,
so a theater provider, a command line and the host's own tests share one client. Documentation:
<https://docs.rs/bohio-client>.

## Features

- `exec::Exec`: run one process, stream its output, write its input, signal it, kill it, wait for its exit.
- `forward`, `socks`, `keep`: a local forward (`ssh -L`), a remote one (`-R`), SOCKS5, jump hosts and a link kept across losses.
- `session`: detached sessions, picked up again from an offset. `search`, `follow` and `walk`: the host's search, a followed read and
  trees. `tools`: the agent tool surface `bohio mcp` serves (`Tools::new(link)`, `Tools::call`).
- `fs::Fs`, `fs::Reader`, `fs::Writer`: stat, list, create, remove, rename, copy files on the host, and `hash` for digests computed there, and
  stream a file in either direction in chunks.
- `shell::Shell`: an interactive shell on a terminal: type, read output, resize, kill.
- `control::Control`: the control stream the small calls share; `call_many` (and `submit` with `collect`) sends many calls before reading
  any answer, so a long path costs one round trip for the batch. `connect::Connection::fs` keeps one such stream warm.
- `parallel`: one file over several streams and links, `get_parallel`, `put_parallel` and `read_parallel`, each range its own stream with
  its own receive window.
- `connect::Connection`: a proven connection over TCP (or any coqui channel) that is itself an `Open`: the host and this side
  prove keys to each other with `cabuya`, and every operation above then runs on a stream of that link.
- Blocking calls with explicit timeouts; every method takes `&self`, so reading and writing can
  run on different threads.
- Errors keep the host's structured `{code, message}` apart from channel failures.

## Installation

```text
cargo add bohio-client --git https://github.com/Boriken-Dev/bohio
```

Cargo features: `zstd` (decode zstd) and `screen` (the compositor of a desktop watch); neither is on by default.

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
| `forward`, `socks` | local and remote forwards, `connect`, SOCKS5 |
| `session`, `search`, `follow`, `tools` | detached sessions, host-side search, followed reads, the agent tools |

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
