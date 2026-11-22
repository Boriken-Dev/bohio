# `bohio-transfer` — API header

Header-file-style reference for the `bohio-transfer` crate: the byte-level work of Bohío, with no I/O of its own (it takes the bytes a caller feeds it, or a
`Read` a caller passes). It ships inside the package and is self-contained. Development documentation lives with the source at
<https://github.com/Boriken-Dev/bohio>.

MSRV Rust 1.89. Depends on `bohio-proto` (the `Algorithm` and `Codec` names), `sha2`, `sha1`, `md-5` (RustCrypto), `blake3`, `xxhash-rust`, `crc32c`, `lz4_flex`, `flate2` and, with the default Cargo feature `zstd`, `zstd` (the C library). Features: `zstd` (default), `zstdmt`.
Version `0.0.0-alpha.0`, not published. `unsafe` is forbidden in this crate; the digest crates' own code is theirs. Public modules: `hash`, `compress`, `rolling`, `signature`, `delta`, `apply`.

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

## Deltas (`rolling`, `signature`, `delta`, `apply`)

rsync's algorithm, streamed, specified in `docs/protocol.md` "Deltas" (no maintained crate does it without whole files in memory: `fast_rsync` takes buffers and MD4 only).

```rust
pub struct Rolling;                                         // Copy; the weak checksum: a = sum x[i], b = sum (L - i) x[i], both mod 2^16, value a + 2^16 b
impl Rolling { pub fn new(window: &[u8]) -> Self; pub fn roll(&mut self, out: u8, incoming: u8); pub const fn value(&self) -> u32; }
pub fn block_size_for(size: u64, min: u32, max: u32) -> u32;   // clamp(next_power_of_two(isqrt(size)), min, max); min wins over a smaller max
pub use bohio_proto::fs::WEAK_RSYNC;
pub struct SignatureSpec { pub block_size: u32, pub strong: Algorithm, pub strong_len: usize }   // SignatureSpec::new(..) checks; digest_len(&Algorithm); strong_of(&[u8]) -> Vec<u8>
pub struct BlockSig { pub weak: u32, pub strong: Vec<u8> }
pub struct SignatureBuilder;                                // new(spec); update(&[u8]) -> Vec<BlockSig> (the blocks the bytes complete); finish() -> Option<BlockSig> (the short last block); holds one block
pub struct Signature;                                       // new(spec, size); push(&BlockSig); push_batch(&[u32], &[u8]); blocks(), blocks_needed(), is_complete(), wire_len(), last_len(); PartialEq
pub struct Matcher;                                         // new(Signature, frame) (must be complete); feed(&[u8]) -> Vec<Op>; finish(self) -> Vec<Op>
pub enum Op { Copy { block: u64, count: u64 }, Literal(Vec<u8>) }    // copies merged; literals at most `frame`; among equal blocks the one that continues the last copy
pub fn apply(base: &mut (impl Read + Seek), op: &Op, block_size: u32, out: &mut impl Write) -> Result<u64, ApplyError>;   // PastEnd for a copy the base lacks
pub fn copy_len(base_len: u64, block_size: u32, block: u64, count: u64) -> Option<u64>;   // the bytes a copy is; None past the end (the last block may be short)
#[non_exhaustive] pub enum SpecError { BlockSize, Algorithm(Algorithm), StrongLen(usize), Weak(String), Batch, TooMany, Incomplete }
```

Memory is the signature (`4 + strong_len` bytes a block, plus 8 for the matcher's index), a window of one block, the pieces not yet matched and one literal of at most `frame` bytes; never the file.
`tests/delta.rs` holds the property test (random edits against random files, block sizes 2 KiB to 1 MiB, the weak sum against its definition).

## Compress (`bohio_transfer::compress`)

```rust
pub fn available() -> Vec<Codec>;                          // lz4, deflate and, with the `zstd` feature, zstd
pub const MULTITHREAD: bool;                               // the `zstdmt` feature: Caps::workers above 0 needs it
pub struct Caps { pub max_level_zstd: i32 /* 19 */, pub max_level_deflate: i32 /* 9 */, pub window_log_max: u32 /* 23 */, pub workers: u32 /* 0 */ }   // Default
impl Caps { pub fn describe(&self, &Codec) -> Option<CompressionInfo>; pub fn clamp(&self, &Compression) -> Result<Compression, Unsupported>; }   // what a host reports it used
pub struct Unsupported(pub Codec);                         // none, auto, an unknown name, or zstd without the feature
pub struct Encoder;                                        // one stream direction
impl Encoder {
    pub fn new(&Compression, &Caps) -> Result<Self, Unsupported>;   pub fn used(&self) -> &Compression;
    pub fn piece(&mut self, input: &[u8], flush: bool) -> Result<Vec<u8>, EncodeError>;   // flush: the piece decodes completely; without it the codec may hold data back
    pub fn finish(self) -> Result<Vec<u8>, EncodeError>;    // the held-back bytes and the end of the frame or stream
    pub fn restart(&mut self);                               // drop the context: the next piece starts a new frame
    pub fn frame(&mut self, input: &[u8]) -> Result<Piece, EncodeError>;   // Compressed(piece) when smaller, else Raw (and restart): the sender then sends a raw frame
}
pub enum Piece { Compressed(Vec<u8>), Raw }
pub struct Bounds { pub window_log_max: u32 /* 27 */, pub max_output: Option<u64>, pub max_expansion: u32 /* 0: off */ }   // Default
pub struct Decoder;
impl Decoder {
    pub fn new(&Compression, &Bounds) -> Result<Self, DecodeError>;
    pub fn feed(&mut self, piece: &[u8], out: &mut dyn Write) -> Result<u64, DecodeError>;   // pieces may be cut anywhere
    pub fn reset(&mut self);                                 // for every raw frame received; the bounds' totals are kept
    pub const fn produced(&self) -> u64;
}
#[non_exhaustive] pub enum DecodeError { Corrupt, TooLong, Expansion, Unsupported(Unsupported), Setup(String), Write(io::ErrorKind) }
pub fn ratio(&Compression, &Caps, sample: &[u8]) -> Result<f64, Unsupported>;                  // compressed over raw size of a sample
pub fn compress_all(&Compression, &Caps, input: &[u8]) -> Result<Vec<u8>, CompressAllError>;   // one context over the whole input: what a prepared transfer sends
pub const FREE_OUTPUT: u64;                                 // 1 MiB: output a decoder produces before it compares with `max_expansion`
```

The streams are the standard formats: zstd frames (the C library; levels below zero are its fast modes), LZ4 frames (independent blocks, no checksums, written
and read here piece by piece; a decoder takes frames with a block or content checksum and skips them), and zlib streams (RFC 1950, a sync flush at the end of each piece, `flate2`
on miniz_oxide). **A raw frame ends the compression context**: `Encoder::frame` restarts the encoder when it falls back to raw, so the next piece begins a new
frame or stream, and a receiver calls `Decoder::reset` for every raw frame. A decoder refuses a zstd frame whose window is above `Bounds::window_log_max`
before it allocates for it, and stops at `max_output` and at `max_expansion` times its input. Cargo features: `zstd` (default; the C library, compiled with `cc`),
`zstdmt` (worker threads).
