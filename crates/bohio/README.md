# bohio

[![Version](https://img.shields.io/crates/v/bohio.svg)](https://crates.io/crates/bohio)
[![Rust version](https://img.shields.io/crates/msrv/bohio.svg)](https://crates.io/crates/bohio)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio)](https://boriken-dev.github.io/bohio/rust/bohio/)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

Bohío is a remote host runtime: **an authenticated peer runs commands, reads files and opens
shells on a machine without SSH, WinRM or Python**, over any coqui transport. This crate is the
host: a library of engines, each serving one operation over a channel, and the `bohio` command.
Documentation: <https://boriken-dev.github.io/bohio/rust/bohio/>.

## Features

- Process execution: a program and its arguments run directly, never through a shell unless the
  caller names one; output streams as it is written; a deadline, a kill request or a closed
  stream ends the whole process tree.
- A program list (`exec.programs`) limits which program starts, not what that program does: each listed name is
  resolved on the host's own `PATH` to an absolute path and that path is started, and a request may not set or
  remove `PATH` or clear the environment. `git`, `make`, `find` and interpreters still run whatever they are given,
  so list only programs that are safe with every argument.
- An authenticated link over TCP (`bohio serve`, `bohio pair`, `bohio join`, `bohio remote`): each side proves a key to the
  other with [`cabuya`](https://github.com/Boriken-Dev/cabuya) (Noise by default); a key the host has not accepted is
  refused and waits for `bohio peer approve`; each stream a peer opens is decided from the peer's `[peer.<id>]` table in `bohio.cfg` when it opens and every
  `[link] recheck` while it runs, so a revoke ends running processes, shells and transfers.
- Memory per link is bounded by `[link]` settings (a credit window per stream, a queue per direction, a read-ahead), never by
  the size of a file.
- Interactive shells on a pseudo-terminal (ConPTY on Windows): output streams as it is produced, the
  terminal can be resized, and a closed stream ends the shell and everything it started.
- Filesystem access confined to the peer's granted roots: a `..` or a symbolic link that leaves a root is refused,
  every path component is resolved against a directory handle, and files stream in chunks with atomic replace.
- Per-peer grants (`[peer.<id>]` in `bohio.cfg`, layered: the system file, then the user's): an accepted peer with no `allow` gets nothing.
  Beyond `info`, `fs.read`, `fs.write`, `exec` and `shell`, a grant can carry `session`, `forward.connect` and `forward.listen`
  (each with the targets or binds it covers), and the desktop, power and media permissions.
- Forwarding (`-L`, `-R`, `-D`, `connect`, `listen`, `--jump`), forwards a node keeps across link losses, detached sessions, signals,
  search, `stats`, `which` and `read --follow` on the host, `--format json` output and named hosts, and `bohio mcp` for agents.
- Threads, not an async runtime: one thread per stream, bounded by configuration.
- Every bound is a setting with a documented default; nothing is hard-coded.

## Installation

```text
cargo add bohio --git https://github.com/Boriken-Dev/bohio
```

| Feature | Adds | Default |
| --- | --- | --- |
| `cli` | the `bohio` command and the `bohio::cli` module | on |
| `zstd` | Zstandard compression through the C library (`lz4` and `deflate` remain without it) | on |
| `zstdmt` | zstd worker threads (`[compression] workers` above 0) | off |
| `serial` | the `serial` terminal provider | on |
| `ssh` | the `ssh` provider: a machine's terminal, processes and files over SSH | on |
| `gui` | snapshot encoding and a watch's tile encodings for `bohio-gui/1` | on |
| `gui-wayland` | the Linux Wayland desktop (portal and PipeWire) | off |
| `notify` | the desktop notice of the GUI presence indicator | on with `cli` |

An embedding host takes the library alone with `default-features = false`.

## Quick start

Grant a peer `exec` and serve one request over an in-process channel:

```rust
use std::sync::Arc;

use bohio::config::{CompressionConfig, ExecutionConfig};
use bohio::access::Sources;
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
let path = std::env::var_os("PATH").unwrap_or_default();
let compression = CompressionConfig::default();
let sources = Sources::default();
let context = Context { grant: &grant, config: &config, compression: &compression, slots: &slots, peer: "peer", path: &path, sources: &sources };

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
`--config` (repeatable) or `$BOHIO_CONFIG`, the system's `bohio.cfg` is read and the user's merged
over it, and with neither the defaults apply.

`bohio serve [--config F] [--listen ENDPOINT]... [--stdio]` serves the `[[listen]]` endpoints (`tcp://`, `pipe://`, `ws://`, `wss://`, `webtransport://`, `serial://`)
and each `--listen` and prints `listening <endpoint>` and `host <id>` once bound; `--stdio` serves one link on standard input and output. `bohio status`, `bohio id` and `bohio peer list|approve|revoke` show and change
who may do what; `bohio pair` arms a PIN on the listeners the node already serves and accepts the one peer that joins with it (`bohio join <endpoint>`, over any endpoint, is the other side);
`bohio remote <endpoint> info|exec -- <argv>|read <path>|shell|forward|connect|search|...` uses a host (`bohio remote --help` lists them), and `bohio remote --via <id> ...` the link a running node holds to that peer, whichever side dialed it (`bohio links` lists them). A peer's table with an `endpoint` makes `bohio serve` dial it. See the header (`AGENTS.md`) for every flag.

| Status | Meaning |
| --- | --- |
| 0 | success |
| 1 | the operation failed |
| 2 | the invocation was wrong |

## API overview

| Module | Purpose |
| --- | --- |
| `config` | `bohio.cfg`: `Config`, its sections, `Layers` (the system file and the user's merged) and the `Environment` the data directory is found in |
| `policy` | `[peer.<id>]`: `Peers`, `Grant` and `Permission` |
| `exec` | serves one `bohio-exec/1` stream |
| `info` | this host's capabilities |
| `link` | who may open which protocol: the `Keys` and `Grants` a node asks, their in-memory forms, admission and `NodeEvents` |
| `peer` | the data directory, the identity, `LayeredPeers`, `list`, `approve`, `revoke` (they edit the layers in place) |
| `pair` | PIN pairing on a node's own listeners: the armed PIN and how it ended |
| `local` | the local control endpoint of a running node (`Local`) and its clients (`LocalLink`, `Via`) |
| `node` | `Node`: an embeddable host made from settings, an identity, `Keys` and `Grants` the caller supplies; accepts links, proves peers and runs each stream's engine |
| `limit` | a counted bound on concurrent work |
| `access` | the sources a node reaches a system through, and the provider libraries it loads |
| `control` | calls on a running node |
| `forward` | forwarding and `connect` on the host |
| `fs` | the filesystem engines: read, write, bundles, hash, walk, search, copy and move |
| `gui` | desktop access: snapshots, input, clipboard and the helper |
| `media` | virtual media sessions |
| `power` | power control sessions |
| `profile` | named transfer defaults |
| `secret` | secret references |
| `session` | detached sessions with scrollback |
| `shell` | interactive shells |
| `stats` | file and tree statistics |
| `cli` | the command line (feature `cli`) |

## Development

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

On Linux `--all-features` includes `gui-wayland`, which links the system libpipewire-0.3 (`pipewire-devel` or `libpipewire-0.3-dev`) and needs
libclang. A machine without them runs the same commands with `--features cli,serial,gui,notify,zstd,zstdmt` in place of `--all-features`
(`-p bohio` for the clippy and test lines: the feature names are the `bohio` crate's).

### Releasing

Nothing is published: the workspace depends on git revisions.

## License

MPL-2.0; see [LICENSE](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE).
