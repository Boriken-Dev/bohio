//! Filesystem types: file metadata, listings, and the frames of the two transfer streams.
//!
//! The small calls (`stat`, `list`, `mkdir`, ...) are [`control`](crate::control) calls; a file's
//! bytes travel on their own streams, never inside CBOR. A path is the host's native text: its
//! separator is `info.filesystem.separator`, and a Windows host also accepts `/`.
//!
//! `bohio-read/1`: the open's metadata is a [`ReadRequest`].
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | host to client | `0x01` | [`ReadOutput::Header`] | CBOR [`ReadHeader`] |
//! | host to client | `0x02` | [`ReadOutput::Data`] | raw bytes |
//! | host to client | `0x04` | [`ReadOutput::Done`] | CBOR [`ReadDone`] |
//! | host to client | `0x05` | [`ReadOutput::Error`] | CBOR [`ErrorBody`] |
//! | host to client | `0x06` | [`ReadOutput::Compressed`] | a piece of the compressed stream |
//! | host to client | `0x08` | [`ReadOutput::Progress`] | CBOR [`Progress`] |
//!
//! `bohio-write/1`: the open's metadata is a [`WriteRequest`].
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | host to client | `0x01` | [`WriteOutput::Ready`] | empty |
//! | host to client | `0x04` | [`WriteOutput::Done`] | CBOR [`WriteDone`] |
//! | host to client | `0x05` | [`WriteOutput::Error`] | CBOR [`ErrorBody`] |
//! | client to host | `0x10` | [`WriteInput::Data`] | raw bytes |
//! | client to host | `0x11` | [`WriteInput::End`] | empty |
//! | client to host | `0x14` | [`WriteInput::Compressed`] | a piece of the compressed stream |

use serde::{Deserialize, Serialize};

use crate::compress::{Compression, Progress, Resume};
use crate::error::{Error, ErrorBody};
use crate::hash::{Algorithm, Digests};
use crate::{decode, encode};

/// A point in time: seconds since the Unix epoch (negative before it) and nanoseconds within the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Timestamp {
    /// Whole seconds since 1970-01-01T00:00:00Z.
    pub secs: i64,
    /// Nanoseconds past `secs`, below one billion.
    #[serde(default)]
    pub nanos: u32,
}

/// What a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link (or a Windows junction).
    Symlink,
    /// A device, socket, pipe or anything a newer host reports.
    #[serde(other)]
    Other,
}

/// What a host knows about one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    /// What it is.
    pub kind: FileKind,
    /// Length in bytes (for a directory, whatever the host reports).
    #[serde(default)]
    pub size: u64,
    /// When the content last changed, when the host knows.
    #[serde(default)]
    pub modified: Option<Timestamp>,
    /// When it was last read, when the host knows.
    #[serde(default)]
    pub accessed: Option<Timestamp>,
    /// When it was created, when the host knows.
    #[serde(default)]
    pub created: Option<Timestamp>,
    /// Unix permission bits (`0o7777` at most); absent on a host without them.
    #[serde(default)]
    pub mode: Option<u32>,
    /// Whether writing is refused: the read-only attribute on Windows, no write bit on Unix.
    #[serde(default)]
    pub readonly: bool,
    /// Windows file attributes; absent on other hosts.
    #[serde(default)]
    pub attributes: Option<u32>,
}

/// One name in a directory with its metadata (not following a symbolic link).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The name within the directory.
    pub name: String,
    /// What it is.
    pub metadata: Metadata,
}

/// One page of a directory, in ascending order of name (by bytes of the UTF-8 text).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Listing {
    /// The names of this page.
    #[serde(default)]
    pub entries: Vec<Entry>,
    /// Pass this as `after` to get the next page; absent on the last page.
    #[serde(default)]
    pub next: Option<String>,
    /// Names that are not valid text and so are left out of the listing.
    #[serde(default)]
    pub skipped: u64,
}

/// What a `bohio-read/1` stream asks for: the open's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRequest {
    /// The file.
    pub path: String,
    /// The first byte to send. Default `0`.
    #[serde(default)]
    pub offset: u64,
    /// How many bytes at most; to the end of the file when absent.
    #[serde(default)]
    pub length: Option<u64>,
    /// Digests of the bytes sent, answered in the closing frame. Default none.
    #[serde(default)]
    pub hashes: Vec<Algorithm>,
    /// How the host sends the bytes: absent or `none` is raw `data` frames. The header says what the
    /// host used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<Compression>,
    /// Compress the whole range into a file on the host first, then send it (a slow link pays for
    /// the best ratio once, and the transfer can resume). The host answers `progress` frames while
    /// it works. Needs `compression` and a host that offers it (`info.transfer.prepared`).
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub prepare: bool,
    /// Continue a prepared read from a compressed offset; `path`, `offset`, `length` and
    /// `compression` are then taken from the prepared file, which only the peer that made it may use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
}

impl ReadRequest {
    /// A request for the whole of `path`.
    pub fn whole(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            length: None,
            hashes: Vec::new(),
            compression: None,
            prepare: false,
            resume: None,
        }
    }

    /// The open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the request cannot be written.
    pub fn to_metadata(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a request from an open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not a request.
    pub fn from_metadata(metadata: &[u8]) -> Result<Self, Error> {
        decode(metadata)
    }
}

/// The first frame of a read: what follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadHeader {
    /// The file's size when the read started.
    pub size: u64,
    /// How many data bytes follow in total, after decompression.
    pub length: u64,
    /// What the host used, never `auto`: absent when the data is raw. The level is the one the
    /// host clamped to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<Compression>,
    /// For a prepared read, how many compressed bytes follow in all (from the start, even when the
    /// read resumes later).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressed_length: Option<u64>,
    /// For a prepared read, the name to resume it by, for as long as the host keeps the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl ReadHeader {
    /// A header for raw data.
    pub const fn raw(size: u64, length: u64) -> Self {
        Self {
            size,
            length,
            compression: None,
            compressed_length: None,
            token: None,
        }
    }
}

/// The last frame of a read that succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReadDone {
    /// The digests of the bytes sent that the request named.
    #[serde(default)]
    pub hashes: Digests,
}

/// How a `bohio-write/1` stream puts the bytes in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WriteMode {
    /// Write a temporary sibling and rename it over the target when the data ends, so a reader
    /// sees the old file or the new one and never a part. An existing symbolic link is replaced,
    /// not followed. This is the default.
    #[default]
    Replace,
    /// Create the target, which must not exist, and write it in place; a stream that ends early
    /// removes it.
    Create,
    /// Add to the end of the target, creating it when absent.
    Append,
    /// Write starting at `offset` in the target, creating it when absent, leaving the rest.
    At {
        /// The first byte written.
        offset: u64,
    },
}

/// What a `bohio-write/1` stream does: the open's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRequest {
    /// The file.
    pub path: String,
    /// How the bytes are put in place.
    #[serde(default)]
    pub mode: WriteMode,
    /// Create missing directories above the file.
    #[serde(default)]
    pub parents: bool,
    /// Unix permission bits for a file this stream creates; the host's default when absent. A
    /// replaced file keeps the mode of the one it replaces unless this is given.
    #[serde(default)]
    pub file_mode: Option<u32>,
    /// The digests (lowercase hex) the data must have, by algorithm; only for `replace` and
    /// `create`: after the data ends each is checked, and on a mismatch nothing is put in place.
    #[serde(default)]
    pub expect: Digests,
    /// The digests of the data written to answer in `write-done`. When absent the host's
    /// `[filesystem] write_hashes` applies; an empty list asks for none.
    #[serde(default)]
    pub hashes: Option<Vec<Algorithm>>,
    /// How the client sends the bytes: absent or `none` is raw `data` frames only. The client names
    /// a concrete algorithm (never `auto`) the host offers; the host decodes within its bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<Compression>,
}

impl WriteRequest {
    /// A request to replace `path` with the data that follows.
    pub fn replace(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            mode: WriteMode::Replace,
            parents: false,
            file_mode: None,
            expect: Digests::new(),
            hashes: None,
            compression: None,
        }
    }

    /// The open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the request cannot be written.
    pub fn to_metadata(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a request from an open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not a request.
    pub fn from_metadata(metadata: &[u8]) -> Result<Self, Error> {
        decode(metadata)
    }
}

/// The last frame of a write that succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteDone {
    /// Data bytes written by this stream.
    pub written: u64,
    /// The digests of those bytes the request asked for.
    #[serde(default)]
    pub hashes: Digests,
}

const HEADER: u8 = 0x01;
const READY: u8 = 0x01;
const DATA: u8 = 0x02;
const DONE: u8 = 0x04;
const ERROR: u8 = 0x05;
const WRITE_DATA: u8 = 0x10;
const WRITE_END: u8 = 0x11;
const COMPRESSED: u8 = 0x06;
const PROGRESS: u8 = 0x08;
const WRITE_COMPRESSED: u8 = 0x14;

fn tagged(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(body.len() + 1);
    frame.push(tag);
    frame.extend_from_slice(body);
    frame
}

fn split(frame: &[u8]) -> Result<(u8, &[u8]), Error> {
    frame
        .split_first()
        .map(|(tag, body)| (*tag, body))
        .ok_or(Error::Short)
}

/// A frame of a read, from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadOutput {
    /// The file was opened; the first frame.
    Header(ReadHeader),
    /// Bytes of the file, in order.
    Data(Vec<u8>),
    /// A piece of the compressed stream. A `data` frame ends the compression context (see
    /// `docs/protocol.md`).
    Compressed(Vec<u8>),
    /// The host is still compressing a prepared read.
    Progress(Progress),
    /// Every byte was sent; the last frame.
    Done(ReadDone),
    /// The read failed; the last frame.
    Error(ErrorBody),
}

impl ReadOutput {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Header(body) => tagged(HEADER, &encode(body)?),
            Self::Data(bytes) => tagged(DATA, bytes),
            Self::Compressed(bytes) => tagged(COMPRESSED, bytes),
            Self::Progress(body) => tagged(PROGRESS, &encode(body)?),
            Self::Done(body) => tagged(DONE, &encode(body)?),
            Self::Error(body) => tagged(ERROR, &encode(body)?),
        })
    }

    /// Reads one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know,
    /// [`Error::Decode`] for a CBOR body that is not what its tag says.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = split(frame)?;
        match tag {
            HEADER => decode(body).map(Self::Header),
            DATA => Ok(Self::Data(body.to_vec())),
            COMPRESSED => Ok(Self::Compressed(body.to_vec())),
            PROGRESS => decode(body).map(Self::Progress),
            DONE => decode(body).map(Self::Done),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}

/// A frame of a write, from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteOutput {
    /// The target is open: the client may send data. The first frame.
    Ready,
    /// The data is in place; the last frame.
    Done(WriteDone),
    /// The write failed and nothing was put in place (for `replace` and `create`); the last frame.
    Error(ErrorBody),
}

impl WriteOutput {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Ready => tagged(READY, &[]),
            Self::Done(body) => tagged(DONE, &encode(body)?),
            Self::Error(body) => tagged(ERROR, &encode(body)?),
        })
    }

    /// Reads one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know,
    /// [`Error::Decode`] for a CBOR body that is not what its tag says.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = split(frame)?;
        match tag {
            READY => Ok(Self::Ready),
            DONE => decode(body).map(Self::Done),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}

/// A frame of a write, from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteInput {
    /// Bytes for the file, in order.
    Data(Vec<u8>),
    /// A piece of the compressed stream. A `data` frame ends the compression context.
    Compressed(Vec<u8>),
    /// No more data: put it in place.
    End,
}

impl WriteInput {
    /// The frame as one stream message.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Data(bytes) => tagged(WRITE_DATA, bytes),
            Self::Compressed(bytes) => tagged(WRITE_COMPRESSED, bytes),
            Self::End => tagged(WRITE_END, &[]),
        }
    }

    /// Reads one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = split(frame)?;
        match tag {
            WRITE_DATA => Ok(Self::Data(body.to_vec())),
            WRITE_COMPRESSED => Ok(Self::Compressed(body.to_vec())),
            WRITE_END => Ok(Self::End),
            other => Err(Error::Tag(other)),
        }
    }
}
