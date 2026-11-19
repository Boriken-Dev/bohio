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
//! | host to client | `0x04` | [`ReadOutput::Done`] | empty |
//! | host to client | `0x05` | [`ReadOutput::Error`] | CBOR [`ErrorBody`] |
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

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorBody};
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
}

impl ReadRequest {
    /// A request for the whole of `path`.
    pub fn whole(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            length: None,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadHeader {
    /// The file's size when the read started.
    pub size: u64,
    /// How many data bytes follow in total.
    pub length: u64,
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
    /// The SHA-256 (lowercase hex) the data must have; only for `replace` and `create`: on a
    /// mismatch nothing is put in place.
    #[serde(default)]
    pub expect_sha256: Option<String>,
}

impl WriteRequest {
    /// A request to replace `path` with the data that follows.
    pub fn replace(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            mode: WriteMode::Replace,
            parents: false,
            file_mode: None,
            expect_sha256: None,
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
    /// SHA-256 (lowercase hex) of those bytes.
    pub sha256: String,
}

const HEADER: u8 = 0x01;
const READY: u8 = 0x01;
const DATA: u8 = 0x02;
const DONE: u8 = 0x04;
const ERROR: u8 = 0x05;
const WRITE_DATA: u8 = 0x10;
const WRITE_END: u8 = 0x11;

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
    /// Every byte was sent; the last frame.
    Done,
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
            Self::Done => tagged(DONE, &[]),
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
            DONE => Ok(Self::Done),
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
    /// No more data: put it in place.
    End,
}

impl WriteInput {
    /// The frame as one stream message.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Data(bytes) => tagged(WRITE_DATA, bytes),
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
            WRITE_END => Ok(Self::End),
            other => Err(Error::Tag(other)),
        }
    }
}
