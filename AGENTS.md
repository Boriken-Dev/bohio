# Bohío: repository orientation

Contributor guidance for this checkout. Each crate has an `AGENTS.md` API header beside its README.

| Header | Covers |
| --- | --- |
| `crates/bohio-proto/AGENTS.md` | the wire types and the CBOR codec |
| `crates/bohio/AGENTS.md` | the host library, configuration, peer grants and the `bohio` command |

## Layout

One Cargo workspace (`Cargo.toml`, one committed `Cargo.lock`: the repository builds a binary).

| Path | What |
| --- | --- |
| `crates/bohio-proto/` | wire types and codec, no I/O, for any client |
| `crates/bohio/` | host library (configuration, grants, capabilities) and the `bohio` binary behind feature `cli` |
| `examples/bohio.cfg` | every setting with its default |

## Commands

From the repository root:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

## Rules

- Operations are engines: each serves one operation over a `coqui_transport::Channel` with the caller's `Grant`, and is tested
  in-process over `coqui_transport::memory` channels. Nothing in an engine names a transport.
- One stream per operation; the open's metadata is the request. Control messages are CBOR with string keys, bulk bytes raw
  behind a one-byte tag. A client ignores fields it does not know.
- The trust store says who a peer is; `peers.cfg` says what it may do. A peer with no `allow` gets nothing.
- No hard-coded limits: every bound is a `bohio.cfg` setting with a documented default; the protocol's own constants are named
  and documented where they are defined.
- Threads, not Tokio: one thread per stream. Nothing buffers without bound; a full stream makes the sender wait.
- A started process runs in a tree that is always ended: after it exits, when its stream closes and at its deadline. A test
  that leaves a process behind fails.
- Nothing logs a key, a PIN or a token; argument vectors are never logged.
- `unsafe` is forbidden in every crate.
- Only text and images are committed.

## CI and release

Nothing is published yet. The workspace depends on git revisions of the Coqui crates; a tag needs the maintainer's go-ahead.
