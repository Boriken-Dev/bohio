# Bohío: repository orientation

Contributor guidance for this checkout. Each crate has an `AGENTS.md` API header beside its README.

| Header | Covers |
| --- | --- |
| `crates/bohio-proto/AGENTS.md` | the wire types and the CBOR codec |
| `crates/bohio-transfer/AGENTS.md` | the digests, computed in one pass, and the compression codecs (no I/O of its own) |
| `crates/bohio-client/AGENTS.md` | the Rust client, the `Open` trait, the proven `Connection` and the bounded `Relay` |
| `crates/bohio/AGENTS.md` | the host library, configuration, peer grants and the `bohio` command |

## Layout

One Cargo workspace (`Cargo.toml`, one committed `Cargo.lock`: the repository builds a binary).

| Path | What |
| --- | --- |
| `crates/bohio-proto/` | wire types and codec, no I/O, for any client |
| `crates/bohio-transfer/` | byte-level work with no I/O: digests of ten algorithms in one pass, and zstd, LZ4 and deflate compression |
| `crates/bohio-client/` | Rust client over a `coqui_transport::Channel`; the link is an `Open`, a caller's or `connect::Connection` |
| `crates/bohio/` | host library (engines, `platform`, configuration, grants, `link`, `peer`, `serve`) and the `bohio` binary behind feature `cli` |
| `crates/bohio-bench/` | the benchmark harness (`publish = false`): scenarios over a link, a raw TCP control, result files |
| `benchmarks/` | `README.md` (schema, scenarios, reproduce), `netem.sh` (emulated links), `scenarios.sh`, `summarize.py`, committed `results/` |
| `examples/bohio.cfg` | every setting with its default |
| `docs/`, `mkdocs.yml` | the narrative site: the protocol (the wire, in CDDL), security and the hostctl mapping; the Rust API reference is published under `/rust/` |
| `vectors/` | the wire's messages as hex, written and checked by `crates/bohio-proto/tests/vectors.rs`; a clean tree after `cargo test` means they are current |
| `tools/` | `build.py`, a copy of the release overlay's build tool (edited there, never here; `self check` holds it equal), its `build.cfg` and `sdk.lock`: the `deps` branch, the SDK lock, offline and release builds |
| `packaging/` | the systemd unit (`systemd/bohio.service`) and the container image (`container/Containerfile`); the Windows service is `bohio service` |

## Commands

From the repository root:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

```bash
python tools/build.py self check        # the copy equals the overlay's
python tools/build.py sdk check --online # the lock agrees with the repository and with its publishers
python tools/build.py deps check         # the deps checkout covers Cargo.lock (--ref SHA for a commit)
python tools/build.py scan REF           # no archive, executable, key or opaque binary
python tools/build.py deps refresh       # after Cargo.lock changes: deps commits, then the pin; push deps before main
```

`docs/building.md` says what each is for; flags are in `--help`.

A narrow run of an integration test that starts the helper program needs it built first, because a test run builds examples
under another name: `cargo build -p bohio --example exec_helper`, then `cargo test -p bohio --test exec`. `tests/link.rs` also runs the
built `bohio` binary (cargo builds it for the test): `bohio serve` runs as a child on `127.0.0.1:0` and the file takes about a minute.

## Rules

- Operations are engines: each serves one operation over a `coqui_transport::Channel` with the caller's `Grant`, and is tested
  in-process over `coqui_transport::memory` channels. Nothing in an engine names a transport.
- One stream per operation; the open's metadata is the request. Control messages are CBOR with string keys, bulk bytes raw
  behind a one-byte tag. A client ignores fields it does not know.
- A peer is a `[peer.<id>]` table in `bohio.cfg` (layered: the system file, then the user's, merged by `config::Layers`); the table
  accepts the key and `allow` says what it may do. A peer with no `allow` gets nothing. Each stream is decided from the layers when it
  opens and every `[link] recheck` while it runs (`link::admission`, over the `Keys` and `Grants` a `node::Node` is given); a revoke ends
  what is running. The data directory holds state only: the identity and the waiting list.
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

- `.github/workflows/test.yml` runs fmt, clippy, the workspace tests, a check that `vectors/` is unchanged and strict rustdoc on Linux,
  Windows and macOS, on stable and 1.89, on demand and on `ci-*` tags. Linux installs `socat` so the serial test runs.
- `test.yml` also runs `tools` (the copy, the SDK lock and the pinned deps commit), `offline` (a build from the deps branch in a
  namespace with no network, beside a control that must fail) and the release builds: a static musl archive per Linux
  architecture, Windows x64 and ARM64 archives, a universal macOS archive and a container image per architecture, each uploaded as an
  artifact. The matrix is `python tools/build.py release matrix`; CI never commits the deps branch.
- `.github/workflows/release.yml` (a `v*` tag, or on demand without publishing) calls `test.yml`, builds the docs strictly without
  deploying, then attaches every artifact and one `SHA256SUMS` to a GitHub release; nothing goes to a registry.
- `.github/workflows/bench.yml` is on demand only (`workflow_dispatch`: `ref`, `baseline_ref`, `samples`, `links`): it measures on emulated links, alternating two refs, with a raw TCP control; never per push.
- `.github/workflows/docs.yml` builds the narrative site strictly with the Rust API reference and deploys Pages.

Nothing is published to a registry. The workspace depends on git revisions of the Coqui crates; a `v*` tag needs the maintainer's go-ahead.
