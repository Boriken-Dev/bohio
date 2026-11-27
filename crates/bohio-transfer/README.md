# bohio-transfer

[![Version](https://img.shields.io/crates/v/bohio-transfer.svg)](https://crates.io/crates/bohio-transfer)
[![Rust version](https://img.shields.io/crates/msrv/bohio-transfer.svg)](https://crates.io/crates/bohio-transfer)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-transfer)](https://boriken-dev.github.io/bohio/rust/bohio_transfer/)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

The byte-level work of Bohío, a remote host runtime: digests of several algorithms in one pass, compression (zstd, LZ4 and deflate) piece by piece, and rsync's rolling checksum, block signatures and deltas. **It does no I/O of
its own**: it takes the bytes a caller feeds it, or a `Read` a caller passes. Documentation: <https://boriken-dev.github.io/bohio/rust/bohio_transfer/>.

## Features

- Ten algorithms by the names the wire uses (`bohio_proto::hash::Algorithm`): `md5`, `sha1`, `sha256`, `sha384`,
  `sha512`, `blake3`, `xxh3`, `xxh128`, `xxh64` and `crc32c`, each written as the reference tools print it.
- Several algorithms in one pass over the bytes.

## Installation

```text
cargo add bohio-transfer --git https://github.com/Boriken-Dev/bohio
```

Features: `zstd` (default) compiles the zstd C library with `cc`; `zstdmt` adds its worker threads. Without `zstd`, `lz4` and `deflate` remain. `screen` adds the `screen` module: the tiles of a changed frame, a rectangle in each of the encodings of a screen stream (RGB with zstd, raw, JPEG, PNG) and the client frame that updates build, with PNG and JPEG from `image`.

## Quick start

```rust
use bohio_proto::hash::Algorithm;
use bohio_transfer::hash::Hasher;

let mut hasher = Hasher::new(&[Algorithm::Sha256, Algorithm::Crc32c])?;
hasher.update(b"123456789");
let digests = hasher.finish();
assert_eq!(digests[&Algorithm::Crc32c], "e3069283");
# Ok::<(), bohio_transfer::hash::Unsupported>(())
```

Compression, one frame at a time: a piece that does not shrink goes raw, and a raw frame ends the context.

```rust
use bohio_proto::compress::{Codec, Compression};
use bohio_transfer::compress::{Bounds, Caps, Decoder, Encoder, Piece};

let request = Compression::new(Codec::Deflate).level(6);
let mut encoder = Encoder::new(&request, &Caps::default())?;
let mut decoder = Decoder::new(&request, &Bounds::default())?;
let chunk = b"hello hello hello hello hello hello\n";
let mut out = Vec::new();
match encoder.frame(chunk)? {
    Piece::Compressed(piece) => { decoder.feed(&piece, &mut out)?; }
    Piece::Raw => { decoder.reset(); out.extend_from_slice(chunk); }
}
assert_eq!(out, chunk);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## API overview

| Module | Purpose |
| --- | --- |
| `hash` | digests of ten algorithms in one pass |
| `compress` | zstd, LZ4 and deflate, piece by piece or in one call |
| `rolling`, `signature`, `delta`, `apply` | rsync's rolling checksum, block signatures, the matcher and the patch |
| `screen` (feature `screen`) | the tiles of a changed frame and the encodings of a screen stream |

## Development

```text
cargo fmt --all --check
cargo clippy -p bohio-transfer --all-targets -- -D warnings
cargo test -p bohio-transfer
RUSTDOCFLAGS="-D warnings" cargo doc -p bohio-transfer --no-deps
```

### Releasing

Nothing is published to a registry: the workspace depends on git revisions, and a `v*` tag of the repository builds the release
archives.

## License

MPL-2.0.
