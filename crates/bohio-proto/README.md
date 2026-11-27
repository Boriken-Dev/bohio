# bohio-proto

[![Version](https://img.shields.io/crates/v/bohio-proto.svg)](https://crates.io/crates/bohio-proto)
[![Rust version](https://img.shields.io/crates/msrv/bohio-proto.svg)](https://crates.io/crates/bohio-proto)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-proto)](https://boriken-dev.github.io/bohio/rust/bohio_proto/)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

The wire types of Bohío, a remote host runtime, and their codec. **It does no I/O and knows no
transport**: a client in any language, or a Rust one over any coqui channel, builds on these
messages. Documentation: <https://boriken-dev.github.io/bohio/rust/bohio_proto/>.

## Features

- One stream per operation, named by a protocol (`bohio/1`, `bohio-exec/1`, ...); the open's
  metadata is the request.
- Control messages are CBOR maps with string keys; a client ignores keys it does not know.
- Bulk bytes (process output, input, file contents) travel as raw stream messages behind a one-byte tag, never
  inside CBOR.
- Errors are `{code, message}` with a closed list of codes.

## Installation

```text
cargo add bohio-proto --git https://github.com/Boriken-Dev/bohio
```

The crate has no Cargo features.

## Quick start

```rust
use bohio_proto::exec::{Output, Request, Started};

# fn main() -> Result<(), bohio_proto::Error> {
let request = Request::argv(["echo", "hello"]);
let metadata = request.to_metadata()?;
assert_eq!(Request::from_metadata(&metadata)?, request);

let frame = Output::Started(Started::new(7)).encode()?;
assert_eq!(frame[0], 0x01);
# Ok(())
# }
```

## API overview

| Module | Purpose |
| --- | --- |
| `codec` | `encode` and `decode` for CBOR, with a nesting bound |
| `control` | `bohio/1` requests, responses and replies |
| `error` | the codec's `Error` and the structured `ErrorBody` with its `ErrorCode` |
| `exec` | `bohio-exec/1`: the request, and the frames in both directions |
| `hash` | `bohio-hash/1`: the algorithm names, the request, items, results and frames |
| `info` | the host's capabilities |
| `protocol` | the protocol names a stream is opened with |
| `shell` | shell kinds |
| `fs`, `walk`, `search` | `bohio-read/1`, `bohio-write/1`, `bohio-walk/1` and `bohio-search/1` |
| `forward` | `bohio-connect/1`, `bohio-listen/1` and `bohio-accept/1`: targets, binds and answers |
| `session`, `signal` | detached sessions (`bohio-attach/1`) and the signals of `exec` and `shell` |
| `gui`, `power`, `media` | the desktop, power and virtual-media protocols |

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
