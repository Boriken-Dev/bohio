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

- **Execution**: a program and its arguments run directly, never through a shell unless the caller
  names one; output streams as it is written; a deadline, a kill request or a closed stream ends
  the whole process tree.
  A program list limits which program starts, not what it does: a listed `git` can still run any alias it is told.
- **Per-peer grants**: who a peer is comes from the trust store; what it may do (`info`,
  `fs.read`, `fs.write`, `exec`, `shell`, `admin`, filesystem roots, a program list) comes from
  `peers.cfg`. A peer with no grant gets nothing.
- **One stream per operation** with a versioned, language-independent protocol: control messages
  are CBOR; bulk bytes travel raw behind a one-byte tag.
- **Threads, not an async runtime**: one thread per stream, bounded by configuration.
- **No hard-coded limits**: every bound is a setting in `bohio.cfg` with a documented default.
- **A proven link**: each side holds a key and proves it to the other (Noise by default) over TCP with
  [`coqui-link`](https://github.com/Boriken-Dev/coqui-link); a key the host has not accepted waits for
  `bohio peer approve`. Memory per link is bounded by settings, never by the size of a file.

The filesystem, interactive shells and the authenticated link are built, over TCP, a named pipe or Unix socket,
a WebSocket, a serial line and standard input and output (see the endpoints below).

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
| `bohio-client` | a Rust client over any coqui channel |
| `bohio` | the host library and the `bohio` command |

## Quick start

```text
cargo run -p bohio -- info --config examples/bohio.cfg
```

prints this host's capabilities. `examples/bohio.cfg` documents every setting with its default.

To serve a peer, on the host:

```text
bohio serve --listen tcp://0.0.0.0:7620     # prints `listening tcp://...` and `host sha256:...`
bohio pair --listen tcp://0.0.0.0:7621      # prints a PIN; waits for one peer to join with it
```

and on the peer:

```text
bohio join tcp://host:7621 --pin 123456     # each side now keeps the other's key
bohio id                                    # the peer's id, for the host to approve
```

then on the host `bohio peer approve <id> --allow info,exec,fs.read --fs-root /srv/data`, and from the peer
`bohio remote tcp://host:7620 info`, `... exec -- git status`, `... read /srv/data/file`, `... shell`. A peer
that proves a key the host has not accepted is refused and listed by `bohio peer list` as waiting; `bohio peer
revoke <id>` takes everything away and ends what the peer is running within `[link] recheck`.

Every transport is one endpoint scheme; the proof, the grants and the engines are the same for each:

| Endpoint | Transport |
| --- | --- |
| `tcp://host:port` | a TCP socket |
| `pipe://name`, `pipe:///path/to.sock` | a Windows named pipe, a Unix domain socket |
| `ws://host:port` | a WebSocket (binary messages); `wss://` is not served yet |
| `serial://PORT?baud=115200` | a serial line (`serial:///dev/ttyUSB0`, `serial://COM3`), COBS frames with a CRC |
| `bohio serve --stdio` | one link on standard input and output, for `docker exec -i NAME bohio serve --stdio` |
| `stdio://program args`, `bohio remote --command "program args"` | the client starts a program and uses its standard streams |

For example `bohio remote --command "docker exec -i box bohio serve --stdio" --server <host id> info`. A peer is
approved by its id over any transport (`bohio peer approve`); pairing by PIN (`pair`, `join`) is TCP only.

## Command line

```text
bohio config check --config examples/bohio.cfg
bohio info --config examples/bohio.cfg --format json
bohio serve | status | id | pair | join <endpoint>
bohio peer list | approve <id> [--allow ...] | revoke <id>
bohio remote <endpoint> info | exec -- <argv> | read <path> | shell
```

See the [`bohio` crate](https://github.com/Boriken-Dev/bohio/tree/main/crates/bohio) for the
configuration search order, the exit statuses and the environment variables.

## API overview

| Crate | Where to look |
| --- | --- |
| `bohio-proto` | [`crates/bohio-proto/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio-proto/AGENTS.md) |
| `bohio-client` | [`crates/bohio-client/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio-client/AGENTS.md) |
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
