//! Compression, piece by piece: an [`Encoder`] turns the chunks of a stream into pieces that decode
//! as they arrive, and a [`Decoder`] turns them back within bounds a peer cannot talk it out of.
//!
//! Three algorithms (`zstd`, `lz4`, `deflate`, see [`Codec`]) share one shape: one context per stream
//! direction, flushed at every piece so each piece decodes completely, and a piece that is no smaller
//! than its input goes raw instead. **A raw frame ends the compression context**: [`Encoder::frame`]
//! restarts the encoder when it falls back to raw, so the next compressed piece begins a new zstd
//! frame, a new LZ4 frame or a new zlib stream, and [`Decoder::reset`] must be called for every raw
//! frame received. Data that does not compress (an archive, a photo) therefore costs one failed
//! attempt per frame at most, and the sender can stop trying when it sees the ratio.
//!
//! The streams are standard formats a peer in any language reads with its own library: zstd frames,
//! LZ4 frames (independent blocks, no checksums) and zlib streams (RFC 1950) with a sync flush at
//! the end of each piece.

use std::io::{self, Write};

use bohio_proto::compress::{Codec, Compression};
use bohio_proto::info::{CompressionInfo, Levels};
use thiserror::Error;

/// Output a decoder produces freely before it compares what it produced with what it took: the
/// first mebibyte. See [`Bounds::max_expansion`].
pub const FREE_OUTPUT: u64 = 1024 * 1024;

/// The size of the buffer a codec works through, in bytes.
const SCRATCH: usize = 128 * 1024;

/// Whether this build compresses with worker threads (`zstdmt`): `Caps::workers` above `0` needs it.
pub const MULTITHREAD: bool = cfg!(all(feature = "zstd", feature = "zstdmt"));

/// The algorithms this build compresses with, in the order a listing shows them: `zstd` only when the
/// crate's `zstd` feature is on.
pub fn available() -> Vec<Codec> {
    Codec::KNOWN
        .into_iter()
        .filter(|codec| *codec != Codec::Zstd || cfg!(feature = "zstd"))
        .collect()
}

/// An algorithm this build does not compress with: unknown, `none`, `auto` (resolve it first) or `zstd`
/// without the feature.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("this build does not compress with {0}")]
pub struct Unsupported(pub Codec);

/// What the host caps a request to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caps {
    /// The highest zstd level.
    pub max_level_zstd: i32,
    /// The highest deflate level (9 at most).
    pub max_level_deflate: i32,
    /// The largest zstd window, as a power of two. It bounds the window an encoder uses and the one a
    /// decoder accepts (memory per stream is about that window).
    pub window_log_max: u32,
    /// zstd worker threads per stream; `0` compresses in the calling thread. Above `0` needs
    /// [`MULTITHREAD`].
    pub workers: u32,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_level_zstd: 19,
            max_level_deflate: 9,
            window_log_max: 23,
            workers: 0,
        }
    }
}

/// The level zstd uses when a request names none.
pub const ZSTD_DEFAULT_LEVEL: i32 = 3;
/// The level deflate uses when a request names none.
pub const DEFLATE_DEFAULT_LEVEL: i32 = 6;

impl Caps {
    /// What the host offers of `codec` under these caps, for its `info`; `None` for one this build does
    /// not compress with.
    pub fn describe(&self, codec: &Codec) -> Option<CompressionInfo> {
        match codec {
            #[cfg(feature = "zstd")]
            Codec::Zstd => Some(CompressionInfo {
                algorithm: Codec::Zstd,
                levels: Levels {
                    min: *zstd::compression_level_range().start(),
                    max: self.max_level_zstd,
                },
                default_level: ZSTD_DEFAULT_LEVEL.min(self.max_level_zstd),
                window_log_max: self.window_log_max,
            }),
            Codec::Lz4 => Some(CompressionInfo {
                algorithm: Codec::Lz4,
                levels: Levels::default(),
                default_level: 0,
                window_log_max: 0,
            }),
            Codec::Deflate => Some(CompressionInfo {
                algorithm: Codec::Deflate,
                levels: Levels {
                    min: 0,
                    max: self.max_level_deflate.clamp(0, 9),
                },
                default_level: DEFLATE_DEFAULT_LEVEL.min(self.max_level_deflate.clamp(0, 9)),
                window_log_max: 0,
            }),
            _ => None,
        }
    }

    /// What a request becomes under these caps: the level moved into the algorithm's range and the
    /// window under `window_log_max`. This is what the host reports it used.
    ///
    /// # Errors
    ///
    /// [`Unsupported`] for `none`, `auto`, an unknown name, or `zstd` in a build without it.
    pub fn clamp(&self, request: &Compression) -> Result<Compression, Unsupported> {
        let info = self
            .describe(&request.algorithm)
            .ok_or_else(|| Unsupported(request.algorithm.clone()))?;
        Ok(match request.algorithm {
            Codec::Lz4 => Compression::new(Codec::Lz4),
            _ => Compression {
                algorithm: request.algorithm.clone(),
                level: Some(
                    request
                        .level
                        .unwrap_or(info.default_level)
                        .clamp(info.levels.min, info.levels.max),
                ),
                window_log: if request.algorithm == Codec::Zstd {
                    request
                        .window_log
                        .map(|window| window.clamp(10, self.window_log_max.max(10)))
                } else {
                    None
                },
            },
        })
    }
}

/// Why compressing failed: the codec reported an error, which only an allocation failure or a bug
/// causes.
#[derive(Debug, Error)]
#[error("compression failed: {0}")]
pub struct EncodeError(String);

impl EncodeError {
    #[allow(dead_code, reason = "used only when a codec feature is built")]
    fn new(error: impl core::fmt::Display) -> Self {
        Self(error.to_string())
    }
}

impl From<EncodeError> for io::Error {
    fn from(error: EncodeError) -> Self {
        Self::other(error)
    }
}

/// One frame's worth of output from [`Encoder::frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// The piece decodes to the input and is smaller than it: send it as a compressed frame.
    Compressed(Vec<u8>),
    /// Compression did not help: send the input as a raw frame. The encoder has started a new context.
    Raw,
}

/// Compresses the chunks of one stream direction.
pub struct Encoder {
    used: Compression,
    inner: EncoderKind,
}

impl core::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoder")
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

enum EncoderKind {
    #[cfg(feature = "zstd")]
    Zstd(ZstdEncoder),
    Lz4(Lz4Encoder),
    Deflate(DeflateEncoder),
}

impl Encoder {
    /// An encoder for `request` under `caps`.
    ///
    /// # Errors
    ///
    /// [`Unsupported`] as [`Caps::clamp`], or when the codec cannot be set up (`zstd` workers without
    /// the `zstdmt` feature, an allocation that failed).
    pub fn new(request: &Compression, caps: &Caps) -> Result<Self, Unsupported> {
        let used = caps.clamp(request)?;
        let inner = match used.algorithm {
            #[cfg(feature = "zstd")]
            Codec::Zstd => EncoderKind::Zstd(
                ZstdEncoder::new(&used, caps).map_err(|_| Unsupported(used.algorithm.clone()))?,
            ),
            Codec::Lz4 => EncoderKind::Lz4(Lz4Encoder::default()),
            Codec::Deflate => EncoderKind::Deflate(DeflateEncoder::new(&used)),
            _ => return Err(Unsupported(used.algorithm)),
        };
        Ok(Self { used, inner })
    }

    /// What this encoder uses: the request after the caps, as a header reports it.
    pub const fn used(&self) -> &Compression {
        &self.used
    }

    /// Compresses `input` and returns what the codec has ready. With `flush` the piece is complete:
    /// everything fed so far decodes from the pieces returned so far. Without it the codec may hold
    /// data back (it compresses a long input better), and [`Encoder::finish`] returns the rest.
    ///
    /// # Errors
    ///
    /// [`EncodeError`] when the codec fails.
    pub fn piece(&mut self, input: &[u8], flush: bool) -> Result<Vec<u8>, EncodeError> {
        match &mut self.inner {
            #[cfg(feature = "zstd")]
            EncoderKind::Zstd(encoder) => encoder.piece(input, flush),
            EncoderKind::Lz4(encoder) => Ok(encoder.piece(input)),
            EncoderKind::Deflate(encoder) => encoder.piece(input, flush),
        }
    }

    /// Ends the stream: what the codec held back and the end of the frame or stream.
    ///
    /// # Errors
    ///
    /// [`EncodeError`] when the codec fails.
    pub fn finish(mut self) -> Result<Vec<u8>, EncodeError> {
        match &mut self.inner {
            #[cfg(feature = "zstd")]
            EncoderKind::Zstd(encoder) => encoder.finish(),
            EncoderKind::Lz4(encoder) => Ok(encoder.finish()),
            EncoderKind::Deflate(encoder) => encoder.finish(),
        }
    }

    /// Drops the context: the next [`Encoder::piece`] starts a new frame or stream. What a sender does
    /// after it sends a raw frame.
    pub fn restart(&mut self) {
        match &mut self.inner {
            #[cfg(feature = "zstd")]
            EncoderKind::Zstd(encoder) => encoder.restart(),
            EncoderKind::Lz4(encoder) => encoder.restart(),
            EncoderKind::Deflate(encoder) => encoder.restart(),
        }
    }

    /// One frame of a stream: the compressed piece, or [`Piece::Raw`] when it would not be smaller (the
    /// encoder then starts a new context, and the sender sends `input` as a raw frame).
    ///
    /// # Errors
    ///
    /// [`EncodeError`] when the codec fails.
    pub fn frame(&mut self, input: &[u8]) -> Result<Piece, EncodeError> {
        let piece = self.piece(input, true)?;
        if piece.len() < input.len() {
            Ok(Piece::Compressed(piece))
        } else {
            self.restart();
            Ok(Piece::Raw)
        }
    }
}

/// How well `sample` compresses with `request` under `caps`: the compressed size over the size, so
/// `0.5` is half. Used to send a file raw when its start does not compress.
///
/// # Errors
///
/// [`Unsupported`] as [`Encoder::new`], [`EncodeError`] mapped to the same.
pub fn ratio(request: &Compression, caps: &Caps, sample: &[u8]) -> Result<f64, Unsupported> {
    if sample.is_empty() {
        return Ok(1.0);
    }
    let mut encoder = Encoder::new(request, caps)?;
    let algorithm = encoder.used().algorithm.clone();
    let mut size = encoder
        .piece(sample, false)
        .map_err(|_| Unsupported(algorithm.clone()))?
        .len();
    size += encoder.finish().map_err(|_| Unsupported(algorithm))?.len();
    #[allow(
        clippy::cast_precision_loss,
        reason = "a ratio of two sizes is all the sampling needs"
    )]
    Ok(size as f64 / sample.len() as f64)
}

/// Compresses the whole of `input` as one stream: the best the algorithm does at the level, in one
/// context. What a prepared transfer sends.
///
/// # Errors
///
/// [`Unsupported`] as [`Encoder::new`], [`EncodeError`] when the codec fails.
pub fn compress_all(
    request: &Compression,
    caps: &Caps,
    input: &[u8],
) -> Result<Vec<u8>, CompressAllError> {
    let mut encoder = Encoder::new(request, caps)?;
    let mut out = encoder.piece(input, false)?;
    out.extend_from_slice(&encoder.finish()?);
    Ok(out)
}

/// Why [`compress_all`] failed.
#[derive(Debug, Error)]
pub enum CompressAllError {
    /// The algorithm is not available.
    #[error(transparent)]
    Unsupported(#[from] Unsupported),
    /// The codec failed.
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

// ---- zstd -------------------------------------------------------------------------------------

#[cfg(feature = "zstd")]
struct ZstdEncoder {
    encoder: zstd::stream::raw::Encoder<'static>,
    scratch: Vec<u8>,
}

#[cfg(feature = "zstd")]
impl ZstdEncoder {
    fn new(used: &Compression, caps: &Caps) -> io::Result<Self> {
        use zstd::zstd_safe::CParameter;
        let level = used.level.unwrap_or(ZSTD_DEFAULT_LEVEL);
        let mut encoder = zstd::stream::raw::Encoder::new(level)?;
        // A window the level would choose above the cap is lowered to the cap; below it the level's
        // own window stands, which is what keeps level 3 at a couple of mebibytes of memory.
        let window = used
            .window_log
            .or_else(|| (level > 19).then_some(caps.window_log_max));
        if let Some(window) = window {
            encoder.set_parameter(CParameter::WindowLog(window.min(caps.window_log_max)))?;
        }
        if caps.workers > 0 {
            if !MULTITHREAD {
                return Err(io::Error::other("worker threads need the zstdmt feature"));
            }
            encoder.set_parameter(CParameter::NbWorkers(caps.workers))?;
        }
        Ok(Self {
            encoder,
            scratch: vec![0; SCRATCH],
        })
    }

    fn piece(&mut self, input: &[u8], flush: bool) -> Result<Vec<u8>, EncodeError> {
        use zstd::stream::raw::{InBuffer, Operation, OutBuffer};
        let mut out = Vec::new();
        let mut source = InBuffer::around(input);
        while source.pos() < input.len() {
            let mut sink = OutBuffer::around(&mut self.scratch[..]);
            self.encoder
                .run(&mut source, &mut sink)
                .map_err(EncodeError::new)?;
            let produced = sink.pos();
            out.extend_from_slice(&self.scratch[..produced]);
        }
        if flush {
            loop {
                let mut sink = OutBuffer::around(&mut self.scratch[..]);
                let left = self.encoder.flush(&mut sink).map_err(EncodeError::new)?;
                let produced = sink.pos();
                out.extend_from_slice(&self.scratch[..produced]);
                if left == 0 {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<u8>, EncodeError> {
        use zstd::stream::raw::{Operation, OutBuffer};
        let mut out = Vec::new();
        loop {
            let mut sink = OutBuffer::around(&mut self.scratch[..]);
            let left = self
                .encoder
                .finish(&mut sink, true)
                .map_err(EncodeError::new)?;
            let produced = sink.pos();
            out.extend_from_slice(&self.scratch[..produced]);
            if left == 0 {
                return Ok(out);
            }
        }
    }

    fn restart(&mut self) {
        use zstd::stream::raw::Operation;
        // A failure here leaves the context as it was; the next piece then fails loudly.
        let _ = self.encoder.reinit();
    }
}

#[cfg(feature = "zstd")]
struct ZstdDecoder {
    decoder: zstd::stream::raw::Decoder<'static>,
}

#[cfg(feature = "zstd")]
impl ZstdDecoder {
    fn new(bounds: &Bounds) -> Result<Self, DecodeError> {
        use zstd::zstd_safe::DParameter;
        let mut decoder = zstd::stream::raw::Decoder::new().map_err(DecodeError::setup)?;
        decoder
            .set_parameter(DParameter::WindowLogMax(bounds.window_log_max))
            .map_err(DecodeError::setup)?;
        Ok(Self { decoder })
    }

    fn feed(
        &mut self,
        piece: &[u8],
        scratch: &mut [u8],
        budget: &mut Budget,
        out: &mut dyn Write,
    ) -> Result<(), DecodeError> {
        use zstd::stream::raw::{InBuffer, Operation, OutBuffer};
        let mut source = InBuffer::around(piece);
        loop {
            let mut sink = OutBuffer::around(&mut scratch[..]);
            let before = source.pos();
            self.decoder
                .run(&mut source, &mut sink)
                .map_err(|_| DecodeError::Corrupt)?;
            let produced = sink.pos();
            budget.emit(&scratch[..produced], out)?;
            let full = produced == scratch.len();
            if source.pos() >= piece.len() && !full {
                return Ok(());
            }
            if source.pos() == before && produced == 0 {
                return Err(DecodeError::Corrupt);
            }
        }
    }

    fn reset(&mut self) {
        use zstd::stream::raw::Operation;
        let _ = self.decoder.reinit();
    }
}

// ---- lz4 --------------------------------------------------------------------------------------

/// The LZ4 frame magic, little-endian on the wire.
const LZ4_MAGIC: [u8; 4] = [0x04, 0x22, 0x4D, 0x18];
/// FLG: version 01, independent blocks, no block checksum, no content size, no content checksum;
/// BD: blocks of at most 4 MiB; then the header checksum of those two bytes.
const LZ4_HEADER: [u8; 7] = [0x04, 0x22, 0x4D, 0x18, 0x60, 0x70, 0x73];
/// The largest block the header above declares.
const LZ4_BLOCK_MAX: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct Lz4Encoder {
    started: bool,
}

impl Lz4Encoder {
    fn piece(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() / 2 + 16);
        if !self.started {
            out.extend_from_slice(&LZ4_HEADER);
            self.started = true;
        }
        for chunk in input.chunks(LZ4_BLOCK_MAX) {
            let mut block = vec![0_u8; lz4_flex::block::get_maximum_output_size(chunk.len())];
            let size = lz4_flex::block::compress_into(chunk, &mut block).unwrap_or(usize::MAX);
            if size < chunk.len() {
                out.extend_from_slice(&u32_le(size));
                out.extend_from_slice(&block[..size]);
            } else {
                // Stored: the high bit says the block is not compressed.
                out.extend_from_slice(&u32_le(chunk.len() | 0x8000_0000));
                out.extend_from_slice(chunk);
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.started {
            out.extend_from_slice(&LZ4_HEADER);
        }
        out.extend_from_slice(&[0, 0, 0, 0]);
        self.started = false;
        out
    }

    const fn restart(&mut self) {
        self.started = false;
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "a block is at most 4 MiB, which a u32 holds with its flag bit"
)]
const fn u32_le(value: usize) -> [u8; 4] {
    (value as u32).to_le_bytes()
}

/// Reads an LZ4 frame (or several) from pieces of any size.
#[derive(Default)]
struct Lz4Decoder {
    pending: Vec<u8>,
    state: Lz4State,
    block: Vec<u8>,
}

#[derive(Default, Clone, Copy)]
enum Lz4State {
    #[default]
    Header,
    BlockSize {
        block_max: usize,
        block_checksum: bool,
        content_checksum: bool,
    },
    Block {
        size: usize,
        stored: bool,
        block_max: usize,
        block_checksum: bool,
        content_checksum: bool,
    },
    ContentChecksum,
}

impl Lz4Decoder {
    fn feed(
        &mut self,
        piece: &[u8],
        scratch: &mut [u8],
        budget: &mut Budget,
        out: &mut dyn Write,
    ) -> Result<(), DecodeError> {
        let _ = scratch;
        self.pending.extend_from_slice(piece);
        let mut at = 0_usize;
        loop {
            let rest = &self.pending[at..];
            match self.state {
                Lz4State::Header => {
                    if rest.len() < 6 {
                        break;
                    }
                    if rest[..4] != LZ4_MAGIC {
                        return Err(DecodeError::Corrupt);
                    }
                    let (flg, bd) = (rest[4], rest[5]);
                    let mut need = 7_usize;
                    if flg & 0x08 != 0 {
                        need += 8;
                    }
                    if flg & 0x01 != 0 {
                        // A dictionary is a shared secret of the two ends; this stream has none.
                        return Err(DecodeError::Corrupt);
                    }
                    if rest.len() < need {
                        break;
                    }
                    // Version 01 and independent blocks: linked blocks need the previous block as history.
                    if flg >> 6 != 1 || flg & 0x20 == 0 {
                        return Err(DecodeError::Corrupt);
                    }
                    let block_max = match (bd >> 4) & 7 {
                        4 => 64 * 1024,
                        5 => 256 * 1024,
                        6 => 1024 * 1024,
                        7 => LZ4_BLOCK_MAX,
                        _ => return Err(DecodeError::Corrupt),
                    };
                    self.state = Lz4State::BlockSize {
                        block_max,
                        block_checksum: flg & 0x10 != 0,
                        content_checksum: flg & 0x04 != 0,
                    };
                    at += need;
                }
                Lz4State::BlockSize {
                    block_max,
                    block_checksum,
                    content_checksum,
                } => {
                    if rest.len() < 4 {
                        break;
                    }
                    let word = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
                    at += 4;
                    if word == 0 {
                        self.state = if content_checksum {
                            Lz4State::ContentChecksum
                        } else {
                            Lz4State::Header
                        };
                    } else {
                        let size = (word & 0x7fff_ffff) as usize;
                        if size > block_max {
                            return Err(DecodeError::Corrupt);
                        }
                        self.state = Lz4State::Block {
                            size,
                            stored: word & 0x8000_0000 != 0,
                            block_max,
                            block_checksum,
                            content_checksum,
                        };
                    }
                }
                Lz4State::Block {
                    size,
                    stored,
                    block_max,
                    block_checksum,
                    content_checksum,
                } => {
                    let total = size + if block_checksum { 4 } else { 0 };
                    if rest.len() < total {
                        break;
                    }
                    let data = &rest[..size];
                    if stored {
                        budget.emit(data, out)?;
                    } else {
                        if self.block.len() < block_max {
                            self.block.resize(block_max, 0);
                        }
                        let produced = lz4_flex::block::decompress_into(data, &mut self.block)
                            .map_err(|_| DecodeError::Corrupt)?;
                        budget.emit(&self.block[..produced], out)?;
                    }
                    at += total;
                    self.state = Lz4State::BlockSize {
                        block_max,
                        block_checksum,
                        content_checksum,
                    };
                }
                Lz4State::ContentChecksum => {
                    if rest.len() < 4 {
                        break;
                    }
                    at += 4;
                    self.state = Lz4State::Header;
                }
            }
        }
        self.pending.drain(..at);
        Ok(())
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.state = Lz4State::Header;
    }
}

// ---- deflate ----------------------------------------------------------------------------------

struct DeflateEncoder {
    compress: flate2::Compress,
    scratch: Vec<u8>,
}

impl DeflateEncoder {
    fn new(used: &Compression) -> Self {
        let level = used.level.unwrap_or(DEFLATE_DEFAULT_LEVEL).clamp(0, 9);
        Self {
            compress: flate2::Compress::new(flate2::Compression::new(level.unsigned_abs()), true),
            scratch: vec![0; SCRATCH],
        }
    }

    fn run(
        &mut self,
        input: &[u8],
        mode: flate2::FlushCompress,
        out: &mut Vec<u8>,
    ) -> Result<bool, EncodeError> {
        let out_before = self.compress.total_out();
        let status = self
            .compress
            .compress(input, &mut self.scratch, mode)
            .map_err(EncodeError::new)?;
        let produced = usize::try_from(self.compress.total_out() - out_before).unwrap_or(0);
        out.extend_from_slice(&self.scratch[..produced]);
        Ok(matches!(status, flate2::Status::StreamEnd) || produced < self.scratch.len())
    }

    fn piece(&mut self, input: &[u8], flush: bool) -> Result<Vec<u8>, EncodeError> {
        let mut out = Vec::new();
        let mut at = 0_usize;
        while at < input.len() {
            let before = self.compress.total_in();
            self.run(&input[at..], flate2::FlushCompress::None, &mut out)?;
            at += usize::try_from(self.compress.total_in() - before).unwrap_or(0);
        }
        if flush {
            while !self.run(&[], flate2::FlushCompress::Sync, &mut out)? {}
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<u8>, EncodeError> {
        let mut out = Vec::new();
        loop {
            let before = self.compress.total_out();
            let status = self
                .compress
                .compress(&[], &mut self.scratch, flate2::FlushCompress::Finish)
                .map_err(EncodeError::new)?;
            let produced = usize::try_from(self.compress.total_out() - before).unwrap_or(0);
            out.extend_from_slice(&self.scratch[..produced]);
            if matches!(status, flate2::Status::StreamEnd) {
                return Ok(out);
            }
        }
    }

    fn restart(&mut self) {
        self.compress.reset();
    }
}

struct DeflateDecoder {
    decompress: flate2::Decompress,
}

impl DeflateDecoder {
    fn new() -> Self {
        Self {
            decompress: flate2::Decompress::new(true),
        }
    }

    fn feed(
        &mut self,
        piece: &[u8],
        scratch: &mut [u8],
        budget: &mut Budget,
        out: &mut dyn Write,
    ) -> Result<(), DecodeError> {
        let mut at = 0_usize;
        loop {
            let (in_before, out_before) = (self.decompress.total_in(), self.decompress.total_out());
            let status = self
                .decompress
                .decompress(&piece[at..], scratch, flate2::FlushDecompress::None)
                .map_err(|_| DecodeError::Corrupt)?;
            let consumed = usize::try_from(self.decompress.total_in() - in_before).unwrap_or(0);
            let produced = usize::try_from(self.decompress.total_out() - out_before).unwrap_or(0);
            at += consumed;
            budget.emit(&scratch[..produced], out)?;
            if matches!(status, flate2::Status::StreamEnd) {
                return if at < piece.len() {
                    Err(DecodeError::Corrupt)
                } else {
                    Ok(())
                };
            }
            let full = produced == scratch.len();
            if at >= piece.len() && !full {
                return Ok(());
            }
            if consumed == 0 && produced == 0 {
                return Err(DecodeError::Corrupt);
            }
        }
    }

    fn reset(&mut self) {
        self.decompress.reset(true);
    }
}

// ---- decoding ---------------------------------------------------------------------------------

/// What a decoder holds a stream to, whatever its peer sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// The largest zstd window a frame may ask for, as a power of two; a frame asking for more is
    /// refused before any memory is spent on it.
    pub window_log_max: u32,
    /// The most bytes the decoder may produce in all (a reader sets it to the header's `length`);
    /// `None` for no limit.
    pub max_output: Option<u64>,
    /// How many times its compressed input the output may grow to, checked once the output passes
    /// [`FREE_OUTPUT`]; `0` turns the check off. A real file can compress better than any fixed ratio
    /// (zeros do 1000 to 1), so a host that must accept those sets it to `0`.
    pub max_expansion: u32,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            window_log_max: 27,
            max_output: None,
            max_expansion: 0,
        }
    }
}

/// Why a decoder refused a stream.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The bytes are not a valid stream of the algorithm, or ask for a window above the bound.
    #[error("the compressed data is not valid")]
    Corrupt,
    /// The output passed `max_output`.
    #[error("the data is longer than it was announced to be")]
    TooLong,
    /// The output passed `max_expansion` times the input.
    #[error("the data expands further than allowed")]
    Expansion,
    /// The algorithm cannot be decoded here.
    #[error(transparent)]
    Unsupported(#[from] Unsupported),
    /// The decoder could not be set up.
    #[error("the decoder cannot start: {0}")]
    Setup(String),
    /// The destination refused the decoded bytes.
    #[error("the decoded bytes cannot be written: {0}")]
    Write(io::ErrorKind),
}

impl DecodeError {
    #[allow(dead_code, reason = "used only when a codec feature is built")]
    fn setup(error: impl core::fmt::Display) -> Self {
        Self::Setup(error.to_string())
    }
}

/// The running totals the bounds are held to.
struct Budget {
    bounds: Bounds,
    taken: u64,
    produced: u64,
}

impl Budget {
    fn emit(&mut self, bytes: &[u8], out: &mut dyn Write) -> Result<(), DecodeError> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.produced += bytes.len() as u64;
        if self
            .bounds
            .max_output
            .is_some_and(|max| self.produced > max)
        {
            return Err(DecodeError::TooLong);
        }
        if self.bounds.max_expansion > 0
            && self.produced > FREE_OUTPUT
            && self.produced
                > self
                    .taken
                    .saturating_mul(u64::from(self.bounds.max_expansion))
        {
            return Err(DecodeError::Expansion);
        }
        out.write_all(bytes)
            .map_err(|error| DecodeError::Write(error.kind()))
    }
}

enum DecoderKind {
    #[cfg(feature = "zstd")]
    Zstd(ZstdDecoder),
    Lz4(Lz4Decoder),
    Deflate(DeflateDecoder),
}

/// Decompresses the pieces of one stream direction, within [`Bounds`].
pub struct Decoder {
    inner: DecoderKind,
    budget: Budget,
    scratch: Vec<u8>,
}

impl core::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decoder")
            .field("produced", &self.budget.produced)
            .finish_non_exhaustive()
    }
}

impl Decoder {
    /// A decoder for what `used` names (a header's compression, or the one a request declared).
    ///
    /// # Errors
    ///
    /// [`Unsupported`] (as [`DecodeError::Unsupported`]) for `none`, `auto`, an unknown name or `zstd`
    /// in a build without it.
    pub fn new(used: &Compression, bounds: &Bounds) -> Result<Self, DecodeError> {
        let inner = match used.algorithm {
            #[cfg(feature = "zstd")]
            Codec::Zstd => DecoderKind::Zstd(ZstdDecoder::new(bounds)?),
            Codec::Lz4 => DecoderKind::Lz4(Lz4Decoder::default()),
            Codec::Deflate => DecoderKind::Deflate(DeflateDecoder::new()),
            _ => return Err(Unsupported(used.algorithm.clone()).into()),
        };
        Ok(Self {
            inner,
            budget: Budget {
                bounds: *bounds,
                taken: 0,
                produced: 0,
            },
            scratch: vec![0; SCRATCH],
        })
    }

    /// Decodes one piece (or any part of the stream: the pieces of a prepared stream may be cut
    /// anywhere), writing what it yields to `out`; returns how many bytes that was.
    ///
    /// # Errors
    ///
    /// [`DecodeError`]: the data is not valid, or passed a bound, or `out` failed.
    pub fn feed(&mut self, piece: &[u8], out: &mut dyn Write) -> Result<u64, DecodeError> {
        self.budget.taken += piece.len() as u64;
        let before = self.budget.produced;
        let budget = &mut self.budget;
        let scratch = &mut self.scratch;
        match &mut self.inner {
            #[cfg(feature = "zstd")]
            DecoderKind::Zstd(decoder) => decoder.feed(piece, scratch, budget, out)?,
            DecoderKind::Lz4(decoder) => decoder.feed(piece, scratch, budget, out)?,
            DecoderKind::Deflate(decoder) => decoder.feed(piece, scratch, budget, out)?,
        }
        Ok(self.budget.produced - before)
    }

    /// Drops the context: the next piece starts a new frame or stream. Call it for every raw frame
    /// received. The totals the bounds are checked against are kept, so alternating raw and compressed
    /// frames does not reset the expansion check.
    pub fn reset(&mut self) {
        match &mut self.inner {
            #[cfg(feature = "zstd")]
            DecoderKind::Zstd(decoder) => decoder.reset(),
            DecoderKind::Lz4(decoder) => decoder.reset(),
            DecoderKind::Deflate(decoder) => decoder.reset(),
        }
    }

    /// How many bytes this decoder has produced in all.
    pub const fn produced(&self) -> u64 {
        self.budget.produced
    }
}
