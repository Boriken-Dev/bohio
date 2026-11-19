# Bohío: repository orientation

Contributor guidance for this checkout. Each crate has an `AGENTS.md` API header beside its README.

| Header | Covers |
| --- | --- |
| `crates/bohio-proto/AGENTS.md` | the wire types and the CBOR codec |
| `crates/bohio-client/AGENTS.md` | the Rust client, the `Open` trait, the proven `Connection` and the bounded `Relay` |
| `crates/bohio/AGENTS.md` | the host library, configuration, peer grants and the `bohio` command |

## Layout

One Cargo workspace (`Cargo.toml`, one committed `Cargo.lock`: the repository builds a binary).

| Path | What |
| --- | --- |
| `crates/bohio-proto/` | wire types and codec, no I/O, for any client |
| `crates/bohio-client/` | Rust client over a `coqui_transport::Channel`; the link is an `Open`, a caller's or `connect::Connection` |
| `crates/bohio/` | host library (engines, `platform`, configuration, grants, `link`, `peer`, `serve`) and the `bohio` binary behind feature `cli` |
| `examples/bohio.cfg` | every setting with its default |

## Commands

From the repository root:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

A narrow run of an integration test that starts the helper program needs it built first, because a test run builds examples
under another name: `cargo build -p bohio --example exec_helper`, then `cargo test -p bohio --test exec`. `tests/link.rs` also runs the
built `bohio` binary (cargo builds it for the test): `bohio serve` runs as a child on `127.0.0.1:0` and the file takes about a minute.

## Rules

- Operations are engines: each serves one operation over a `coqui_transport::Channel` with the caller's `Grant`, and is tested
  in-process over `coqui_transport::memory` channels. Nothing in an engine names a transport.
- One stream per operation; the open's metadata is the request. Control messages are CBOR with string keys, bulk bytes raw
  behind a one-byte tag. A client ignores fields it does not know.
- The trust store says who a peer is; `peers.cfg` says what it may do. A peer with no `allow` gets nothing. Each stream is decided from
  `peers.cfg` when it opens and every `[link] recheck` while it runs (`link::admission`); a revoke ends what is running.
- No hard-coded limits: every bound is a `bohio.cfg` setting with a documented default; the protocol's own constants are named
  and documented where they are defined.
- Threads, not Tokio: one thread per stream. Nothing buffers without bound; a full stream makes the sender wait. A link stream borrows
  its link, so an engine and a client reach it through a `bohio_client::relay::Relay` whose queues are `[link]` settings.
- A started process runs in a tree that is always ended: after it exits, when its stream closes and at its deadline. A test
  that leaves a process behind fails.
- Tests that start processes use the `exec_helper` example, read with timeouts, and kill what they start.
- Nothing logs a key, a PIN or a token; argument vectors are never logged.
- `unsafe` is forbidden in every crate.
- Only text and images are committed.

## CI and release

Nothing is published yet. The workspace depends on git revisions of the Coqui crates; a tag needs the maintainer's go-ahead.
