# bohio-transfer

[![Version](https://img.shields.io/crates/v/bohio-transfer.svg)](https://crates.io/crates/bohio-transfer)
[![Rust version](https://img.shields.io/crates/msrv/bohio-transfer.svg)](https://crates.io/crates/bohio-transfer)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-transfer)](https://docs.rs/bohio-transfer)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

The byte-level work of Bohío, a remote host runtime: digests of several algorithms in one pass. **It does no I/O of
its own**: it takes the bytes a caller feeds it, or a `Read` a caller passes. Documentation: <https://docs.rs/bohio-transfer>.

## Features

- Ten algorithms by the names the wire uses (`bohio_proto::hash::Algorithm`): `md5`, `sha1`, `sha256`, `sha384`,
  `sha512`, `blake3`, `xxh3`, `xxh128`, `xxh64` and `crc32c`, each written as the reference tools print it.
- Several algorithms in one pass over the bytes.

## Installation

```text
cargo add bohio-transfer --git https://github.com/Boriken-Dev/bohio
```

The crate has no Cargo features.

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

## License

MPL-2.0.
