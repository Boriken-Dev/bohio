# `bohio-transfer` — API header

Header-file-style reference for the `bohio-transfer` crate: the byte-level work of Bohío, with no I/O of its own (it takes the bytes a caller feeds it, or a
`Read` a caller passes). It ships inside the package and is self-contained. Development documentation lives with the source at
<https://github.com/Boriken-Dev/bohio>.

MSRV Rust 1.89. Depends on `bohio-proto` (the `Algorithm` names), `sha2`, `sha1`, `md-5` (RustCrypto), `blake3`, `xxhash-rust` and `crc32c`. No Cargo features.
Version `0.0.0-alpha.0`, not published. `unsafe` is forbidden in this crate; the digest crates' own code is theirs. Public modules: `hash`.

## Hash (`bohio_transfer::hash`)

```rust
pub fn available() -> Vec<Algorithm>;                       // every algorithm this build computes: Algorithm::KNOWN
pub struct Unsupported(pub Algorithm);                      // Error: an Algorithm::Other
pub struct Hasher;                                          // Debug
impl Hasher {
    pub fn new(algorithms: &[Algorithm]) -> Result<Hasher, Unsupported>;   // one named twice is computed once
    pub fn is_empty(&self) -> bool;
    pub fn update(&mut self, bytes: &[u8]);                 // every algorithm sees the bytes: one pass
    pub fn finish(self) -> BTreeMap<Algorithm, String>;     // Digests: lowercase hex
}
pub fn digest_reader(reader: &mut impl Read, algorithms: &[Algorithm], buffer_size: usize) -> io::Result<(Digests, u64)>;   // digests and bytes read
```

Hex is in the byte order the reference tools print (`md5sum`, `sha256sum`, `b3sum`, `xxhsum`): the digest's bytes for `md5`, `sha1`, `sha256`, `sha384`,
`sha512` and `blake3`; the value in big-endian for `xxh3`, `xxh128` (high half first), `xxh64` (seed 0) and `crc32c` (8 digits). `tests/vectors.rs` holds
the published vectors of each: the empty input, `abc` and, for `crc32c`, `123456789` (`e3069283`). `md5` and `sha1` are not collision resistant.

`blake3` compiles C or assembly through `cc` for its SIMD code unless built with its `pure` feature, which this crate does not set.
