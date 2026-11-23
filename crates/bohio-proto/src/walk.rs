//! `bohio-walk/1`: a tree's entries on one stream.
//!
//! The open's metadata is a [`WalkRequest`]. The client then sends one [`WalkInput::Root`] per tree (and may
//! send more while the host answers) and a [`WalkInput::End`]; the host answers each root in order with batches
//! of entries, depth first and each directory's names sorted by the bytes of their UTF-8 text.
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | host to client | `0x01` | [`WalkOutput::Header`] | CBOR [`WalkHeader`] |
//! | host to client | `0x02` | [`WalkOutput::Entries`] | CBOR [`WalkEntries`] |
//! | host to client | `0x03` | [`WalkOutput::ItemError`] | CBOR [`WalkItemError`] |
//! | host to client | `0x04` | [`WalkOutput::Done`] | CBOR [`WalkDone`] |
//! | host to client | `0x05` | [`WalkOutput::Error`] | CBOR [`ErrorBody`] |
//! | host to client | `0x06` | [`WalkOutput::Compressed`] | a piece of the compressed stream: it decodes to one `entries` body |
//! | client to host | `0x10` | [`WalkInput::Root`] | CBOR [`WalkRoot`] |
//! | client to host | `0x11` | [`WalkInput::End`] | empty |

use serde::{Deserialize, Serialize};

use crate::compress::Compression;
use crate::error::{Error, ErrorBody, ErrorCode};
use crate::fs::Metadata;
use crate::hash::{Algorithm, Digests};
use crate::{decode, encode};

/// What a `bohio-walk/1` stream does: the open's metadata. A walk of the whole tree under a root, links
/// described and not followed, is the default.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WalkRequest {
    /// How far below a root to go: `0` is the root alone (a batched `stat`), `1` adds its names, and
    /// absent is the whole tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u64>,
    /// Enter a symbolic link to a directory and describe what a link to a file points at, as long as the
    /// target is inside the peer's root; a directory already entered (by device and inode) is not
    /// entered twice. A host that cannot tell directories apart does not enter a link to one.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub follow: bool,
    /// Only entries whose path (below the root, with `/`) or name matches one of these globs are listed;
    /// every directory is still entered. All entries when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Entries whose path or name matches one of these globs are left out, and a directory's whole subtree
    /// with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Digests of each regular file's content, computed on the host (they read every file, under `[hash]`'s rate).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hashes: Vec<Algorithm>,
    /// How the host sends the entries: absent or `none` is `entries` frames. The header says what the host used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<Compression>,
}

impl WalkRequest {
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

/// The first frame of a walk: what the host does.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WalkHeader {
    /// What the host used, never `auto`: absent when the entries are raw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<Compression>,
}

/// One tree to walk: the client's `0x10` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkRoot {
    /// The directory (or file) the walk starts at.
    pub path: String,
}

/// One name found: its path below the root and what the host knows of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkEntry {
    /// The path below the root with `/` between names; empty for the root itself, which is the first entry
    /// of its walk.
    pub path: String,
    /// What it is (a link is described itself, not what it points at).
    pub metadata: Metadata,
    /// For a symbolic link, the text it holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The digests of a regular file's content, when the request asked for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashes: Option<Digests>,
}

/// A batch of entries of one root: the host's `0x02` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkEntries {
    /// Which root: its position among the client's roots, from `0`.
    pub root: u64,
    /// The entries, in walk order.
    #[serde(default)]
    pub entries: Vec<WalkEntry>,
}

/// Something that could not be read: the host's `0x03` frame. The walk goes on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkItemError {
    /// Which root.
    pub root: u64,
    /// The path below the root it concerns; absent for the root itself or for the stream's own limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// What kind of failure.
    pub code: ErrorCode,
    /// A sentence for a person; it never carries a secret.
    #[serde(default)]
    pub message: String,
}

impl WalkItemError {
    /// The error as a plain body.
    pub fn body(&self) -> ErrorBody {
        ErrorBody::new(self.code, self.message.clone())
    }
}

/// The end of a walk: the host's `0x04` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WalkDone {
    /// How many entries were sent.
    pub entries: u64,
    /// Names that are not valid text and so are left out.
    #[serde(default)]
    pub skipped: u64,
}

const HEADER: u8 = 0x01;
const ENTRIES: u8 = 0x02;
const ITEM_ERROR: u8 = 0x03;
const DONE: u8 = 0x04;
const ERROR: u8 = 0x05;
const COMPRESSED: u8 = 0x06;
const ROOT: u8 = 0x10;
const END: u8 = 0x11;

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

/// A frame of a walk, from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WalkOutput {
    /// The stream was accepted; the first frame.
    Header(WalkHeader),
    /// A batch of entries.
    Entries(WalkEntries),
    /// A piece of the compressed stream: it decodes to the body of one `entries` frame. An `entries` frame
    /// ends the compression context.
    Compressed(Vec<u8>),
    /// A directory or root could not be read, or the walk reached the host's bound.
    ItemError(WalkItemError),
    /// Every root was walked; the last frame.
    Done(WalkDone),
    /// The walk failed as a whole; the last frame.
    Error(ErrorBody),
}

impl WalkOutput {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Header(body) => tagged(HEADER, &encode(body)?),
            Self::Entries(body) => tagged(ENTRIES, &encode(body)?),
            Self::Compressed(bytes) => tagged(COMPRESSED, bytes),
            Self::ItemError(body) => tagged(ITEM_ERROR, &encode(body)?),
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
            ENTRIES => decode(body).map(Self::Entries),
            COMPRESSED => Ok(Self::Compressed(body.to_vec())),
            ITEM_ERROR => decode(body).map(Self::ItemError),
            DONE => decode(body).map(Self::Done),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}

/// A frame of a walk, from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WalkInput {
    /// The next tree.
    Root(WalkRoot),
    /// No more roots.
    End,
}

impl WalkInput {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Root(body) => tagged(ROOT, &encode(body)?),
            Self::End => tagged(END, &[]),
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
            ROOT => decode(body).map(Self::Root),
            END => Ok(Self::End),
            other => Err(Error::Tag(other)),
        }
    }
}
