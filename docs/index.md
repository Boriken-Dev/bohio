# Bohío

**A remote host runtime: a proven peer runs commands, reads and writes files and opens shells on a machine without SSH, WinRM or Python**, over TCP, a named pipe, a WebSocket (with TLS), WebTransport, a serial line or standard input and output, on computers, virtual machines and containers.

Three rules hold the design together:

- **A proof admits, a grant allows.** Each side holds a key and proves it to the other. Being proven opens nothing: the peer's `[peer.<id>]` table in `bohio.cfg` says what it may do, and every stream is decided again while it runs, so a revoke ends it.
- **One stream per operation.** A client opens a stream, names a protocol and sends the request as the open's metadata. Control messages are CBOR and file or process bytes travel raw, behind a one-byte tag, so a client in any language can speak it.
- **No hard-coded limits.** Every bound is a setting in `bohio.cfg` with a documented default, and nothing buffers without bound.

## Installation

```bash
git clone https://github.com/Boriken-Dev/bohio
cd bohio
cargo build --release
```

Nothing is published to a registry yet: the workspace depends on git revisions of the Coqui crates.

## 30-second tour

On the host:

```bash
bohio serve --listen tcp://0.0.0.0:7620
bohio pair
```

On a peer, with the PIN `pair` shows:

```bash
bohio join tcp://host.example:7620 --pin 123456
bohio id
```

Back on the host, approve the peer by the id it printed and say what it may do:

```bash
bohio peer approve <id> --allow info,exec,fs.read --fs-root /srv/data
```

Then from the peer:

```bash
bohio remote tcp://host.example:7620 info
bohio remote tcp://host.example:7620 exec -- git status
bohio remote tcp://host.example:7620 read /srv/data/file
```

For a container, `bohio serve --stdio` serves one link on standard input and output, and a peer reaches it with `bohio remote --command "docker exec -i box bohio serve --stdio" info`.

## A host behind a firewall

A host that nobody can reach dials out instead, and is controlled over the link it made. On the controller, which listens at `controller.example` (`192.0.2.10`):

```bash
bohio serve --listen tcp://0.0.0.0:7620
bohio id
```

On the host, a table for the controller's id with an `endpoint` makes `bohio serve` keep a link to it, with nothing to listen on; `allow` is what the controller may do here:

```toml
[peer."sha256:<controller id>"]
endpoint = "tcp://controller.example:7620"
allow = ["info", "exec", "fs.read"]
fs_roots = ["/srv/data"]
```

The controller sees the host's key waiting (`bohio peer list`), accepts it with `bohio peer approve <host id>` (no `--allow`: it grants the host nothing), and the next attempt links. The controller's own node then holds the link, so it is controlled through that node's local endpoint:

```bash
bohio links
bohio remote --via <host id> exec -- hostname
```

Who dialed decides nothing: each side's tables say what the other may do there. [Security](security.md#links-in-both-directions) has the rest.

## Reaching a service through a host

A host can connect to a target for a peer, from its own network position: a database on its network, a container engine's socket, another machine's `ssh`. The host grants it by target, not by "network access": `forward.connect` and a rule for each target.

```bash
bohio peer approve <id> --connect db.example:5432 --connect 192.0.2.0/24:22 --connect unix:/run/docker.sock
```

From the peer, `-L` listens on this machine and has the host connect for each connection (the local side is `127.0.0.1` unless another address is named), and `connect` bridges the target to standard input and output, so it is `netcat` and `ssh`'s `ProxyCommand`:

```bash
bohio remote tcp://host.example:7620 forward -L 5433=db.example:5432 -L unix:/tmp/docker.sock=unix:/run/docker.sock
psql -h 127.0.0.1 -p 5433 app

ssh -o ProxyCommand="bohio remote tcp://host.example:7620 connect %h:%p" ops@192.0.2.20
bohio remote tcp://host.example:7620 connect --check db.example:5432   # connected 192.0.2.30:5432
```

A target the grant does not cover is refused before the host connects to anything. [The protocol](protocol.md#forwarding-bohio-connect1-and-tcp1) has the wire and the rules, and [Security](security.md#what-a-forward-gives-a-peer) what a forward gives a peer.

## Configuration

`bohio.cfg` is read as layers: the system file (`/etc/bohio/bohio.cfg`, `%ProgramData%\bohio\bohio.cfg`) and then the user's (`~/.config/bohio/bohio.cfg`, `%APPDATA%\bohio\bohio.cfg`) merged over it, or the files `--config` names, alone. Tables merge by key and lists and values replace, so a higher layer can narrow a list. Every setting, with its default, is in [`examples/bohio.cfg`](https://github.com/Boriken-Dev/bohio/blob/main/examples/bohio.cfg); the `[peer.*]` tables are read again at every decision, the rest when the node starts.

## Learn more

- [The protocol](protocol.md): the wire, for an implementer in any language, with test vectors.
- [Security](security.md): what the service holds, what a peer is granted, where isolation stops, and the known limits.
- [Embedding a node](embedding.md): running a node inside another application, from its own settings, key and peers.
- [Using it from hostctl](hostctl.md): how a hostctl provider maps `run`, paths, shells and capability selection onto Bohío.
- [The Rust API reference](https://boriken-dev.github.io/bohio/rust/): every public item of `bohio`, `bohio-client` and `bohio-proto`.
