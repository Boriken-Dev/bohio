# bohio

[![Version](https://img.shields.io/crates/v/bohio.svg)](https://crates.io/crates/bohio)
[![Rust version](https://img.shields.io/crates/msrv/bohio.svg)](https://crates.io/crates/bohio)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio)](https://boriken-dev.github.io/bohio/)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

Bohío is a remote host runtime. **A proven peer runs commands, reads files and opens shells on a
machine without SSH, WinRM or Python**, over any coqui transport, on physical computers, virtual
machines and containers. It is an application: the `coqui-*` names are reserved for libraries.

## Features

- **Execution**: a program and its arguments run directly, never through a shell unless the caller
  names one; output streams as it is written; a deadline, a kill request or a closed stream ends
  the whole process tree.
  A program list limits which program starts, not what it does: a listed `git` can still run any alias it is told.
- **Per-peer grants**: a peer is a `[peer.<id>]` table in `bohio.cfg`, which accepts its key; what it may do
  (`info`, `fs.read`, `fs.write`, `exec`, `shell`, `session`, `forward.connect`, `forward.listen`, `admin`, the desktop,
  power and media permissions, filesystem roots, a program list, the targets and binds a forward may use) is the table's
  `allow` and its keys. A peer with no `allow` gets nothing. `bohio.cfg` is layered: the system file, then the
  user's merged over it.
- **Forwarding, like ssh**: `-L`, `-R`, `-D` (SOCKS5), `connect` for `ProxyCommand`, jump hosts (`--jump`), and
  forwards a node keeps up across link losses; every kind of end (TCP, Unix socket, named pipe, standard input and output)
  pairs with every other, each target decided by a rule in the peer's grant.
- **Programs that survive a lost link**: detached sessions with scrollback, signals to a running program, and a program on
  a terminal with a deadline.
- **Desktops**: a peer with the desktop permissions sees and drives a screen (`bohio-gui/1`): snapshots, watches, input and
  clipboard, on X11, Wayland, Windows and macOS, on a machine reached over SSH, and through a per-user helper for a node that
  runs as a service.
- **Power and virtual media**: sources of power control and of boot or install media, granted per peer like the rest.
- **Providers**: a terminal, a desktop, power, media, processes and files are capabilities that sources serve; `native` is
  one, `serial` and `ssh` ship in the tree, and others are libraries (`[providers] paths`) or programs (`[[providers.program]]`).
- **Transfer efficiency**: compression (zstd, lz4, deflate), delta transfers, `sync` of trees, and named profiles
- **Search, `stats`, `which` and `read --follow`** on the host for a peer that only reads, and `--format json` for every command.
- **Agent tools**: `bohio mcp` serves a host's files, programs and sessions to any Model Context Protocol client,
  bounded by the key's grant; `bohio_client::tools` is the same surface for an embedding agent.
- **One stream per operation** with a versioned, language-independent protocol: control messages
  are CBOR; bulk bytes travel raw behind a one-byte tag.
- **Threads, not an async runtime**: one thread per stream, bounded by configuration.
- **No hard-coded limits**: every bound is a setting in `bohio.cfg` with a documented default.
- **A proven link**: each side holds a key and proves it to the other (Noise by default) over TCP with
  [`cabuya`](https://github.com/Boriken-Dev/cabuya); a key the host has not accepted waits for
  `bohio peer approve`. Memory per link is bounded by settings, never by the size of a file.

The filesystem, interactive shells and the authenticated link are built, over TCP, a named pipe or Unix socket,
a WebSocket (with TLS), WebTransport, a serial line and standard input and output (see the endpoints below).

## Installation

The workspace builds from source; nothing is published yet.

```text
git clone https://github.com/Boriken-Dev/bohio
cd bohio
cargo build --release
```

`tools/build.py` builds without a network from the `deps` branch and makes the release archives per target;
[docs/building.md](https://github.com/Boriken-Dev/bohio/blob/main/docs/building.md) says how.

| Crate | Purpose |
| --- | --- |
| `bohio-proto` | the wire types and their CBOR codec; no I/O |
| `bohio-client` | a Rust client over any coqui channel |
| `bohio` | the host library and the `bohio` command |
| `bohio-api` | the provider kinds (terminal, desktop, power, media, exec, files) and their contract suites |
| `bohio-transfer` | byte-level work with no I/O: digests, compression, the delta matcher, screen encodings |
| `bohio-x11` | the X11 desktop backend, which runs over any stream |
| `bohio-provider-ssh` | the `ssh` provider: a machine's terminal, processes and files over SSH; `providers/ssh/plugin` is the same provider as a library |
| `bohio-provider-serial` | the `serial` terminal provider; `providers/terminal/serial/plugin` is the same provider as a library |
| `bohio-bench` | the benchmark harness |

## Quick start

```text
cargo run -p bohio -- info --config examples/bohio.cfg
```

prints this host's capabilities. `examples/bohio.cfg` documents every setting with its default.

To serve a peer, on the host:

```text
bohio serve --listen tcp://0.0.0.0:7620     # prints `listening tcp://...` and `host sha256:...`
bohio pair                                  # prints a PIN, armed on the node's own listeners; waits for one peer
```

and on the peer:

```text
bohio join tcp://host:7620 --pin 123456     # any endpoint `remote` takes; each side now keeps the other's key
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
| `ws://host:port`, `wss://host:port` | a WebSocket (binary messages), with TLS for `wss`: the host's `tls_cert` and `tls_key`; a client's `--ca FILE` or `--cert-hash HASH` |
| `webtransport://host:port` | WebTransport over QUIC (UDP), always TLS: files, or a certificate the host makes and prints the hash of; a client's `--fallback ws://...` for a blocked UDP path; `mtu_*` settings (`--mtu-*` flags) for a path of a smaller MTU such as a tunnel |
| `serial://PORT?baud=115200` | a serial line (`serial:///dev/ttyUSB0`, `serial://COM3`), COBS frames with a CRC |
| `bohio serve --stdio` | one link on standard input and output, for `docker exec -i NAME bohio serve --stdio` |
| `stdio://program args`, `bohio remote --command "program args"` | the client starts a program and uses its standard streams |

For example `bohio remote --command "docker exec -i box bohio serve --stdio" --server <host id> info`. A peer is
approved by its id over any transport (`bohio peer approve`), and pairing by PIN (`pair`, `join`) runs on the same listener a node serves, over every transport.

## Command line

```text
bohio config check --config examples/bohio.cfg
bohio info --config examples/bohio.cfg --format json
bohio serve | status | id | pair | join <endpoint>
bohio peer list | approve <id> [--allow ...] | revoke <id>
bohio remote <endpoint> info | exec -- <argv> | read <path> | write <path> | shell | search | glob | stats | which
bohio remote <endpoint> forward -L ... -R ... -D <port> | connect <target> | listen <bind>
bohio remote <endpoint> exec --detach -- <argv> | attach <id> | sessions | end <id>
bohio remote --via <id> info | exec -- <argv> | read <path> | shell
bohio hosts | links
bohio mcp --host <name> [--max-output BYTES]
```

See the [`bohio` crate](https://github.com/Boriken-Dev/bohio/tree/main/crates/bohio) for the
configuration search order, the exit statuses and the environment variables.

## API overview

| Crate | Where to look |
| --- | --- |
| `bohio-proto` | [`crates/bohio-proto/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio-proto/AGENTS.md) |
| `bohio-client` | [`crates/bohio-client/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio-client/AGENTS.md) (the module headers are listed there) |
| `bohio` | [`crates/bohio/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio/AGENTS.md) (the module headers are listed there) |
| `bohio-api` | [`crates/bohio-api/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio-api/AGENTS.md) |
| `bohio-transfer` | [`crates/bohio-transfer/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio-transfer/AGENTS.md) |
| `bohio-x11` | [`crates/bohio-x11/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio-x11/AGENTS.md) |
| `bohio-provider-ssh` | [`providers/ssh/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/providers/ssh/AGENTS.md) |
| `bohio-provider-serial` | [`providers/terminal/serial/AGENTS.md`](https://github.com/Boriken-Dev/bohio/blob/main/providers/terminal/serial/AGENTS.md) |

## Documentation

The site at <https://boriken-dev.github.io/bohio/> has the narrative pages and, under [`/rust/`](https://boriken-dev.github.io/bohio/rust/), the Rust API reference:

- [Overview](https://boriken-dev.github.io/bohio/), [Profiles](https://boriken-dev.github.io/bohio/profiles/) and [Building](https://boriken-dev.github.io/bohio/building/);
- [Protocol](https://boriken-dev.github.io/bohio/protocol/), the wire for an implementer in another language (`vectors/` holds its messages as hex);
- [Security](https://boriken-dev.github.io/bohio/security/), [Sources and providers](https://boriken-dev.github.io/bohio/providers/) and [Agents and MCP](https://boriken-dev.github.io/bohio/agent-tools/);
- [Machine-readable output](https://boriken-dev.github.io/bohio/cli-json/), [embedding a node](https://boriken-dev.github.io/bohio/embedding/) and [hostctl](https://boriken-dev.github.io/bohio/hostctl/).

## Deployment

`packaging/systemd/bohio.service` runs `bohio serve` as the unprivileged user `bohio` under systemd, `bohio service install` registers a
Windows service, and `packaging/container/Containerfile` builds an image whose example entrypoint is `bohio serve --stdio`, from the `deps` branch with no
network after the toolchain (`python tools/build.py container build`; [building](https://boriken-dev.github.io/bohio/building/) lists the `.env` overrides).

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

Nothing is published to a registry: the workspace depends on git revisions. A `v*` tag builds the archives for every
target and the container images and attaches them to a GitHub release.

## License

MPL-2.0; see [LICENSE](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE). Contributors sign
[CLA.md](https://github.com/Boriken-Dev/bohio/blob/main/CLA.md).
