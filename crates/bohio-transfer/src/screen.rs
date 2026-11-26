//! Screen updates, with no I/O: which tiles of a frame changed, a rectangle of pixels in each encoding of `bohio-gui/1`, and
//! the client's [`Compositor`], which holds the frame the updates build.
//!
//! The host runs a [`TileGrid`] over each frame it captures, merges the changed tiles into rectangles and encodes each with
//! [`encode_rect`]. The client feeds every update's header and bytes to [`Compositor::apply`], which refuses what would grow
//! it past its [`Limits`] (a host is not trusted with the client's memory).

use std::collections::HashMap;
use std::io::Cursor;

use bohio_proto::gui::{Rect, RectEncoding, UpdateHeader};
use thiserror::Error;
use xxhash_rust::xxh3::Xxh3;

/// Why an encoding or a decoding was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ScreenError {
    /// The pixel buffer does not hold `width * height * 3` bytes, or a side is zero.
    #[error("the pixels do not match their size")]
    BadPixels,
    /// A rectangle has no area or leaves the frame.
    #[error("a rectangle is not inside the frame")]
    OutsideFrame,
    /// A frame, a rectangle or an update is larger than the limits allow.
    #[error("the update is larger than this side accepts")]
    TooLarge,
    /// The bytes are not what their encoding and rectangle say.
    #[error("a rectangle's bytes are not valid: {0}")]
    Corrupt(&'static str),
    /// The header and the bytes disagree on how long they are, or a key frame does not cover its frame, or a size changed
    /// without a key frame.
    #[error("the update is inconsistent: {0}")]
    Inconsistent(&'static str),
    /// This build does not do the encoding.
    #[error("this build does not do the {0} encoding")]
    Unsupported(RectEncoding),
    /// The encoder failed.
    #[error("the rectangle cannot be encoded")]
    Encode,
}

/// A picture of three bytes a pixel, rows from the top with no padding, borrowed.
#[derive(Debug, Clone, Copy)]
pub struct Pixels<'a> {
    width: u32,
    height: u32,
    rgb: &'a [u8],
}

impl<'a> Pixels<'a> {
    /// `rgb` as a picture of `width` by `height`.
    ///
    /// # Errors
    ///
    /// [`ScreenError::BadPixels`] when a side is zero or `rgb` has another length.
    pub fn new(width: u32, height: u32, rgb: &'a [u8]) -> Result<Self, ScreenError> {
        let expected = u64::from(width) * u64::from(height) * 3;
        if width == 0 || height == 0 || rgb.len() as u64 != expected {
            return Err(ScreenError::BadPixels);
        }
        Ok(Self { width, height, rgb })
    }

    /// Width in pixels.
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The bytes.
    pub const fn rgb(&self) -> &'a [u8] {
        self.rgb
    }

    /// The bytes of `rect`, rows from its top.
    fn rect_rgb(&self, rect: Rect) -> Result<Vec<u8>, ScreenError> {
        if !rect.fits(self.width, self.height) {
            return Err(ScreenError::OutsideFrame);
        }
        let (width, x, row) = (
            self.width as usize,
            rect.x as usize,
            rect.width as usize * 3,
        );
        let mut out = Vec::with_capacity(row * rect.height as usize);
        for y in rect.y as usize..(rect.y + rect.height) as usize {
            let from = (y * width + x) * 3;
            out.extend_from_slice(&self.rgb[from..from + row]);
        }
        Ok(out)
    }
}

/// Which tiles of a frame changed since the one before, by an xxh3 of each tile.
#[derive(Debug, Clone)]
pub struct TileGrid {
    tile: u32,
    width: u32,
    height: u32,
    across: u32,
    hashes: Vec<u64>,
    primed: bool,
}

impl TileGrid {
    /// A grid of `tile` by `tile` pixels (at least 1) over a frame of `width` by `height`. Nothing has been seen yet, so
    /// the first [`TileGrid::diff`] reports every tile.
    pub fn new(width: u32, height: u32, tile: u32) -> Self {
        let tile = tile.max(1);
        let mut grid = Self {
            tile,
            width: 0,
            height: 0,
            across: 0,
            hashes: Vec::new(),
            primed: false,
        };
        grid.resize(width, height);
        grid
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.across = width.div_ceil(self.tile);
        let down = height.div_ceil(self.tile);
        self.hashes = vec![0; self.across as usize * down as usize];
        self.primed = false;
    }

    /// The tile side in pixels.
    pub const fn tile(&self) -> u32 {
        self.tile
    }

    /// How many tiles the grid has.
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /// Whether the grid has no tile (a frame with no area).
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Forgets what was seen: the next [`TileGrid::diff`] reports every tile.
    pub fn reset(&mut self) {
        self.primed = false;
    }

    /// The place of tile `index` in the frame.
    pub fn tile_rect(&self, index: usize) -> Rect {
        let across = self.across as usize;
        let (column, row) = ((index % across) as u32, (index / across) as u32);
        let (x, y) = (column * self.tile, row * self.tile);
        Rect::new(
            x,
            y,
            (x + self.tile).min(self.width) - x,
            (y + self.tile).min(self.height) - y,
        )
    }

    /// The indices of the tiles `rect` touches.
    pub fn tiles_in(&self, rect: Rect) -> Vec<usize> {
        if rect.width == 0 || rect.height == 0 || rect.x >= self.width || rect.y >= self.height {
            return Vec::new();
        }
        let last_x = (rect.x + rect.width - 1).min(self.width - 1) / self.tile;
        let last_y = (rect.y + rect.height - 1).min(self.height - 1) / self.tile;
        let mut tiles = Vec::new();
        for row in rect.y / self.tile..=last_y {
            for column in rect.x / self.tile..=last_x {
                tiles.push((row * self.across + column) as usize);
            }
        }
        tiles
    }

    /// The indices of the tiles of `frame` that differ from the last frame this grid saw, ascending; every tile for the
    /// first frame, after a [`TileGrid::reset`] and when the size changed.
    pub fn update(&mut self, frame: &Pixels<'_>) -> Vec<usize> {
        if (frame.width, frame.height) != (self.width, self.height) {
            self.resize(frame.width, frame.height);
        }
        let (width, tile) = (frame.width as usize, self.tile as usize);
        let across = self.across as usize;
        let mut changed = Vec::new();
        for (index, slot) in self.hashes.iter_mut().enumerate() {
            let (column, row) = (index % across, index / across);
            let (left, top) = (column * tile, row * tile);
            let right = (left + tile).min(width);
            let bottom = (top + tile).min(frame.height as usize);
            let mut hasher = Xxh3::new();
            for y in top..bottom {
                hasher.update(&frame.rgb[(y * width + left) * 3..(y * width + right) * 3]);
            }
            let hash = hasher.digest();
            if !self.primed || *slot != hash {
                changed.push(index);
            }
            *slot = hash;
        }
        self.primed = true;
        changed
    }

    /// The changed rectangles of `frame`: [`TileGrid::update`] with the tiles merged by [`TileGrid::rects`].
    pub fn diff(&mut self, frame: &Pixels<'_>) -> Vec<Rect> {
        let tiles = self.update(frame);
        self.rects(&tiles)
    }

    /// Rectangles that cover exactly `tiles` (ascending, as [`TileGrid::update`] gives them): the tiles of a row that touch
    /// make one run, and runs of the same columns in consecutive rows make one rectangle.
    pub fn rects(&self, tiles: &[usize]) -> Vec<Rect> {
        let across = self.across as usize;
        let mut runs: Vec<(usize, usize, usize)> = Vec::new();
        let mut at = 0;
        while at < tiles.len() {
            let row = tiles[at] / across;
            let first = tiles[at] % across;
            let mut last = first;
            at += 1;
            while at < tiles.len() && tiles[at] / across == row && tiles[at] % across == last + 1 {
                last += 1;
                at += 1;
            }
            runs.push((row, first, last));
        }
        // Rectangles as (first column, last column, first row, last row); `previous` and `current` say which of them end on
        // the row before and on this one, by their columns.
        let mut merged: Vec<(usize, usize, usize, usize)> = Vec::new();
        let mut previous: HashMap<(usize, usize), usize> = HashMap::new();
        let mut current: HashMap<(usize, usize), usize> = HashMap::new();
        let mut current_row: Option<usize> = None;
        for (row, first, last) in runs {
            if current_row != Some(row) {
                previous = if current_row.is_some_and(|before| before + 1 == row) {
                    std::mem::take(&mut current)
                } else {
                    current.clear();
                    HashMap::new()
                };
                current_row = Some(row);
            }
            let index = if let Some(&index) = previous.get(&(first, last)) {
                merged[index].3 = row;
                index
            } else {
                merged.push((first, last, row, row));
                merged.len() - 1
            };
            current.insert((first, last), index);
        }
        merged
            .into_iter()
            .map(|(first, last, top, bottom)| {
                let tile = self.tile as usize;
                let x = first * tile;
                let y = top * tile;
                let right = ((last + 1) * tile).min(self.width as usize);
                let down = ((bottom + 1) * tile).min(self.height as usize);
                Rect::new(x as u32, y as u32, (right - x) as u32, (down - y) as u32)
            })
            .collect()
    }
}

/// How a rectangle is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeParams {
    /// JPEG quality, 1 to 100 (clamped).
    pub quality: u8,
    /// The zstd level of `zstd_rgb`.
    pub zstd_level: i32,
}

impl Default for EncodeParams {
    fn default() -> Self {
        Self {
            quality: 80,
            zstd_level: 3,
        }
    }
}

/// The largest zstd window a `zstd_rgb` rectangle may declare, as a power of two (2^27 = 128 MiB): what the decoder allocates
/// at most, whatever the sender writes. A protocol constant of `bohio-gui/1`: the encoder never needs more for a tile.
pub const ZSTD_WINDOW_LOG_MAX: u32 = 27;

/// Whether this build does `zstd_rgb`.
pub const ZSTD: bool = cfg!(feature = "zstd");

/// The lossless encoding of a build that has no PNG in its way: `zstd_rgb` with zstd, else `raw_rgb`.
pub const fn lossless() -> RectEncoding {
    if ZSTD {
        RectEncoding::ZstdRgb
    } else {
        RectEncoding::RawRgb
    }
}

/// `rect` of `frame` in `encoding`.
///
/// # Errors
///
/// [`ScreenError::OutsideFrame`] for a rectangle that is not inside the frame, [`ScreenError::Unsupported`] for `zstd_rgb`
/// in a build without zstd, [`ScreenError::Encode`] when an image encoder fails.
pub fn encode_rect(
    frame: &Pixels<'_>,
    rect: Rect,
    encoding: RectEncoding,
    params: &EncodeParams,
) -> Result<Vec<u8>, ScreenError> {
    use image::codecs::jpeg::JpegEncoder;
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder};

    let rgb = frame.rect_rgb(rect)?;
    match encoding {
        RectEncoding::RawRgb => Ok(rgb),
        #[cfg(feature = "zstd")]
        RectEncoding::ZstdRgb => {
            use crate::compress::{Caps, compress_all};
            use bohio_proto::compress::{Codec, Compression};
            let request = Compression::new(Codec::Zstd).level(params.zstd_level);
            compress_all(&request, &Caps::default(), &rgb).map_err(|_| ScreenError::Encode)
        }
        #[cfg(not(feature = "zstd"))]
        RectEncoding::ZstdRgb => Err(ScreenError::Unsupported(encoding)),
        RectEncoding::Jpeg => {
            let mut out = Vec::new();
            JpegEncoder::new_with_quality(&mut out, params.quality.clamp(1, 100))
                .write_image(&rgb, rect.width, rect.height, ExtendedColorType::Rgb8)
                .map_err(|_| ScreenError::Encode)?;
            Ok(out)
        }
        RectEncoding::Png => {
            let mut out = Vec::new();
            PngEncoder::new(&mut out)
                .write_image(&rgb, rect.width, rect.height, ExtendedColorType::Rgb8)
                .map_err(|_| ScreenError::Encode)?;
            Ok(out)
        }
    }
}

/// What a client holds an update to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most bytes of one rectangle, and of one update's rectangles in all.
    pub max_rect_bytes: u64,
    /// The most pixels the frame may have.
    pub max_frame_pixels: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rect_bytes: 32 * 1024 * 1024,
            max_frame_pixels: 32 * 1024 * 1024,
        }
    }
}

/// The RGB of `rect` from `bytes` in `encoding`: exactly `width * height * 3` bytes or an error. The size of what an
/// encoder claims is checked before anything is allocated for it.
///
/// # Errors
///
/// [`ScreenError::Corrupt`] for bytes that are not the encoding or hold another size, [`ScreenError::Unsupported`] for
/// `zstd_rgb` in a build without zstd.
pub fn decode_rect(
    bytes: &[u8],
    rect: Rect,
    encoding: RectEncoding,
) -> Result<Vec<u8>, ScreenError> {
    use image::{DynamicImage, ImageDecoder, Limits as ImageLimits};

    let expected = rect.area() * 3;
    match encoding {
        RectEncoding::RawRgb => {
            if bytes.len() as u64 != expected {
                return Err(ScreenError::Corrupt("a raw rectangle of another size"));
            }
            Ok(bytes.to_vec())
        }
        #[cfg(feature = "zstd")]
        RectEncoding::ZstdRgb => {
            use crate::compress::{Bounds, Decoder};
            use bohio_proto::compress::{Codec, Compression};
            let bounds = Bounds {
                window_log_max: ZSTD_WINDOW_LOG_MAX,
                max_output: Some(expected),
                max_expansion: 0,
            };
            let mut decoder = Decoder::new(&Compression::new(Codec::Zstd), &bounds)
                .map_err(|_| ScreenError::Corrupt("zstd cannot start"))?;
            let mut out = Vec::with_capacity(usize::try_from(expected).unwrap_or(0));
            decoder
                .feed(bytes, &mut out)
                .map_err(|_| ScreenError::Corrupt("a zstd rectangle that does not decode"))?;
            if out.len() as u64 != expected {
                return Err(ScreenError::Corrupt("a zstd rectangle of another size"));
            }
            Ok(out)
        }
        #[cfg(not(feature = "zstd"))]
        RectEncoding::ZstdRgb => Err(ScreenError::Unsupported(encoding)),
        RectEncoding::Jpeg | RectEncoding::Png => {
            let limits = || {
                let mut limits = ImageLimits::no_limits();
                limits.max_image_width = Some(rect.width);
                limits.max_image_height = Some(rect.height);
                limits.max_alloc = Some(expected.saturating_mul(4).saturating_add(1 << 20));
                limits
            };
            let wrong = |_| ScreenError::Corrupt("an image that does not decode");
            let cursor = Cursor::new(bytes);
            let image = if encoding == RectEncoding::Jpeg {
                let mut decoder = image::codecs::jpeg::JpegDecoder::new(cursor).map_err(wrong)?;
                if decoder.dimensions() != (rect.width, rect.height) {
                    return Err(ScreenError::Corrupt("an image of another size"));
                }
                decoder.set_limits(limits()).map_err(wrong)?;
                DynamicImage::from_decoder(decoder).map_err(wrong)?
            } else {
                let mut decoder = image::codecs::png::PngDecoder::new(cursor).map_err(wrong)?;
                if decoder.dimensions() != (rect.width, rect.height) {
                    return Err(ScreenError::Corrupt("an image of another size"));
                }
                decoder.set_limits(limits()).map_err(wrong)?;
                DynamicImage::from_decoder(decoder).map_err(wrong)?
            };
            let rgb = image.into_rgb8().into_raw();
            if rgb.len() as u64 != expected {
                return Err(ScreenError::Corrupt("an image of another size"));
            }
            Ok(rgb)
        }
    }
}

/// The client's frame: what updates, applied in order, build.
#[derive(Debug, Clone)]
pub struct Compositor {
    limits: Limits,
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

impl Compositor {
    /// An empty frame; the first update must be a key frame.
    pub const fn new(limits: Limits) -> Self {
        Self {
            limits,
            width: 0,
            height: 0,
            rgb: Vec::new(),
        }
    }

    /// Holds later updates to `limits`; the frame already held stays as it is.
    pub const fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// The frame so far, `None` before the first key frame.
    pub fn frame(&self) -> Option<Pixels<'_>> {
        Pixels::new(self.width, self.height, &self.rgb).ok()
    }

    /// Applies one update: its header and the bytes of its rectangles in order.
    ///
    /// The header is checked whole before any pixel is touched: the frame's size against `max_frame_pixels`, each rectangle
    /// inside the frame and under `max_rect_bytes`, the lengths adding up to `bytes`, a key frame covering the frame, a
    /// size that changed only on a key frame. A rectangle whose bytes then do not decode ends the update with an error and
    /// leaves the frame as the rectangles before it made it.
    ///
    /// # Errors
    ///
    /// [`ScreenError::TooLarge`], [`ScreenError::OutsideFrame`], [`ScreenError::Inconsistent`], [`ScreenError::Corrupt`] or
    /// [`ScreenError::Unsupported`].
    pub fn apply(&mut self, header: &UpdateHeader, bytes: &[u8]) -> Result<(), ScreenError> {
        let (width, height) = (header.width, header.height);
        if width == 0 || height == 0 {
            return Err(ScreenError::BadPixels);
        }
        if u64::from(width) * u64::from(height) > self.limits.max_frame_pixels {
            return Err(ScreenError::TooLarge);
        }
        let resized = (width, height) != (self.width, self.height);
        if resized && !header.key {
            return Err(ScreenError::Inconsistent(
                "the size changed without a key frame",
            ));
        }
        let mut total = 0_u64;
        let mut covered = 0_u64;
        for part in &header.rects {
            if !part.rect().fits(width, height) {
                return Err(ScreenError::OutsideFrame);
            }
            if part.length > self.limits.max_rect_bytes {
                return Err(ScreenError::TooLarge);
            }
            total = total.saturating_add(part.length);
            covered = covered.saturating_add(part.rect().area());
        }
        if total > self.limits.max_rect_bytes {
            return Err(ScreenError::TooLarge);
        }
        if total != bytes.len() as u64 {
            return Err(ScreenError::Inconsistent(
                "the bytes are not the rectangles' lengths",
            ));
        }
        if header.key && covered < u64::from(width) * u64::from(height) {
            return Err(ScreenError::Inconsistent(
                "a key frame that does not cover the frame",
            ));
        }
        if resized {
            self.width = width;
            self.height = height;
            self.rgb = vec![0; width as usize * height as usize * 3];
        }
        let mut offset = 0_usize;
        for part in &header.rects {
            let end = offset + part.length as usize;
            let rect = part.rect();
            let rgb = decode_rect(&bytes[offset..end], rect, part.encoding)?;
            offset = end;
            let row = rect.width as usize * 3;
            for (line, pixels) in rgb.chunks_exact(row).enumerate() {
                let at = ((rect.y as usize + line) * width as usize + rect.x as usize) * 3;
                self.rgb[at..at + row].copy_from_slice(pixels);
            }
        }
        Ok(())
    }
}
