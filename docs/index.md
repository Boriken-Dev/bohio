# Bohío

**A remote host runtime: a proven peer runs commands, reads and writes files and opens shells on a machine without SSH, WinRM or Python**, over TCP, a named pipe, a WebSocket (with TLS), WebTransport, a serial line or standard input and output, on computers, virtual machines and containers.

Three rules hold the design together:

- **A proof admits, a grant allows.** Each side holds a key and proves it to the other. Being proven opens nothing: `peers.cfg` says what each peer may do, and every stream is decided again while it runs, so a revoke ends it.
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
bohio pair --listen tcp://0.0.0.0:7621
```

On a peer, with the PIN `pair` shows:

```bash
bohio join tcp://host.example:7621 --pin 123456
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

## Learn more

- [The protocol](protocol.md): the wire, for an implementer in any language, with test vectors.
- [Security](security.md): what the service holds, what a peer is granted, where isolation stops, and the known limits.
- [Using it from hostctl](hostctl.md): how a hostctl provider maps `run`, paths, shells and capability selection onto Bohío.
- [The Rust API reference](https://boriken-dev.github.io/bohio/rust/): every public item of `bohio`, `bohio-client` and `bohio-proto`.
