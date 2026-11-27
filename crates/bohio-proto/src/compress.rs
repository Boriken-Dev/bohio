//! Compression: the names an algorithm goes by and the options a request carries.
//!
//! A client asks for compression per stream and per direction with a [`Compression`]; the host clamps
//! it to what it allows and reports what it used in the stream's first frame. Compressed bytes travel
//! in their own frame tag beside the raw one (`docs/protocol.md` has the tables and the rules), so a
//! sender switches per frame, and a peer that never asks never sees one.

use core::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A compression algorithm, by the name the wire uses.
///
/// A name this version does not know is [`Codec::Other`], so a newer peer's list still decodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Codec {
    /// Zstandard: levels from fast to very high, long windows, the best ratio for a slow link.
    Zstd,
    /// LZ4: very cheap, for a host whose CPU cannot afford more. It has no levels here.
    Lz4,
    /// Deflate in the zlib framing (RFC 1950): what a browser's `DecompressionStream("deflate")`
    /// and Python's `zlib` decode.
    Deflate,
    /// No compression.
    None,
    /// In a request: the host's configured default. Never in a header or an `info`.
    Auto,
    /// A name this version does not know.
    Other(String),
}

impl Codec {
    /// The algorithms this version names that compress, in the order a listing shows them.
    pub const KNOWN: [Self; 3] = [Self::Zstd, Self::Lz4, Self::Deflate];

    /// The name on the wire.
    pub fn name(&self) -> &str {
        match self {
            Self::Zstd => "zstd",
            Self::Lz4 => "lz4",
            Self::Deflate => "deflate",
            Self::None => "none",
            Self::Auto => "auto",
            Self::Other(name) => name,
        }
    }

    /// The algorithm a name stands for; [`Codec::Other`] when this version does not know it.
    pub fn from_name(name: &str) -> Self {
        match name {
            "zstd" => Self::Zstd,
            "lz4" => Self::Lz4,
            "deflate" => Self::Deflate,
            "none" => Self::None,
            "auto" => Self::Auto,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Whether this is an algorithm that compresses: not `none`, `auto` or a name this version does
    /// not know.
    pub const fn compresses(&self) -> bool {
        matches!(self, Self::Zstd | Self::Lz4 | Self::Deflate)
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Serialize for Codec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Codec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|name| Self::from_name(&name))
    }
}

/// What a request asks of one direction of one stream, or what the host reports it used.
///
/// A request that has none, or `none`, is not compressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compression {
    /// Which algorithm.
    pub algorithm: Codec,
    /// The level, in the algorithm's own scale (zstd 1 to 22 and below zero for the fastest, deflate
    /// 0 to 9); the algorithm's default when absent. A host clamps it to its `max_level` and the
    /// header says what it used. Ignored by `lz4`.
    #[serde(default)]
    pub level: Option<i32>,
    /// zstd's window as a power of two (a long window finds repeats further back, at the cost of
    /// memory on both ends); the host's choice when absent. Clamped to the host's `window_log_max`.
    #[serde(default)]
    pub window_log: Option<u32>,
}

impl Compression {
    /// No compression.
    pub const fn none() -> Self {
        Self::new(Codec::None)
    }

    /// The host's configured default.
    pub const fn auto() -> Self {
        Self::new(Codec::Auto)
    }

    /// `algorithm` at its default level.
    pub const fn new(algorithm: Codec) -> Self {
        Self {
            algorithm,
            level: None,
            window_log: None,
        }
    }

    /// The same compression at `level`.
    #[must_use]
    pub fn level(mut self, level: i32) -> Self {
        self.level = Some(level);
        self
    }

    /// The same compression with a window of `2^window_log` bytes.
    #[must_use]
    pub fn window_log(mut self, window_log: u32) -> Self {
        self.window_log = Some(window_log);
        self
    }

    /// Whether data is compressed at all: the algorithm is one that compresses, or `auto`.
    pub const fn is_requested(&self) -> bool {
        self.algorithm.compresses() || matches!(self.algorithm, Codec::Auto)
    }

    /// Reads `ALG` or `ALG:LEVEL` (`zstd`, `zstd:19`, `deflate:9`, `none`, `auto`).
    ///
    /// # Errors
    ///
    /// [`ParseError::Level`] when the level is not a whole number.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let (name, level) = match text.split_once(':') {
            Some((name, level)) => (name, Some(level)),
            None => (text, None),
        };
        let level = level
            .map(|digits| digits.parse::<i32>().map_err(|_| ParseError::Level))
            .transpose()?;
        Ok(Self {
            algorithm: Codec::from_name(name),
            level,
            window_log: None,
        })
    }
}

impl core::str::FromStr for Compression {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, ParseError> {
        Self::parse(text)
    }
}

/// Why compression text could not be read. It never quotes the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    /// The part after the colon is not a whole number.
    #[error("the compression level is not a whole number")]
    Level,
}

impl Default for Compression {
    fn default() -> Self {
        Self::none()
    }
}

/// Where a prepared read picks up: the token its first header gave and how many compressed bytes the
/// client already holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    /// The `token` of the earlier read's header.
    pub token: String,
    /// The offset in the compressed bytes to continue at.
    pub offset: u64,
}

/// Sent while a host compresses a prepared read: it is working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// How many bytes of the file are compressed so far.
    pub prepared: u64,
}
