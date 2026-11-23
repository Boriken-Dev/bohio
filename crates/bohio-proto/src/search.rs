//! `bohio-search/1`: lines of files that match a pattern, found on the host and sent as matches.
//!
//! The open's metadata is a [`SearchRequest`]. The host walks the roots (the walk of `bohio-walk/1`, links never
//! followed), reads each file it may and matches its lines, so the matches and not the tree cross the link. The client
//! sends nothing after the open; closing the stream stops the search.
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | host to client | `0x01` | [`SearchOutput::File`] | CBOR [`SearchFile`], before a file's first match |
//! | host to client | `0x02` | [`SearchOutput::Match`] | CBOR [`SearchMatch`] of the file last announced |
//! | host to client | `0x03` | [`SearchOutput::ItemError`] | CBOR [`SearchItemError`] |
//! | host to client | `0x04` | [`SearchOutput::Done`] | CBOR [`SearchDone`] |
//! | host to client | `0x05` | [`SearchOutput::Error`] | CBOR [`ErrorBody`] |

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorBody, ErrorCode};
use crate::{decode, encode};

/// Which part of the host's trees a search reads: the options of `bohio-walk/1` that apply to matching.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SearchScope {
    /// The directories (or files) to search, each as a path inside the peer's roots. At least one.
    #[serde(default)]
    pub roots: Vec<String>,
    /// How far below a root to go: `0` is the root alone (a file), `1` adds its names, and absent is the whole tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u64>,
    /// Only files whose path (below the root, with `/`) or name matches one of these globs are read; every directory is
    /// still entered. All files when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Entries whose path or name matches one of these globs are left out, and a directory's whole subtree with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

/// What a `bohio-search/1` stream does: the open's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SearchRequest {
    /// The trees to search.
    #[serde(default)]
    pub walk: SearchScope,
    /// Search names that start with a dot too. Default `false`.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub hidden: bool,
    /// The pattern: a regular expression (linear-time matching, so no pattern can run away), or with `literal` the
    /// text itself. Matched against each line without its line ending. Not empty.
    #[serde(default)]
    pub pattern: String,
    /// Take `pattern` as plain text.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub literal: bool,
    /// Ignore case (Unicode simple case folding).
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub case_insensitive: bool,
    /// Lines to send before and after each match. Default `0`; the host's `[search] max_context` bounds it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context: u64,
    /// The most matches to send, at most the host's `[search] max_matches`. At least 1. Absent asks for the host's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_matches: Option<u64>,
    /// The most files to read, at most the host's `[search] max_files`. At least 1. Absent asks for the host's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files: Option<u64>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl SearchRequest {
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

/// A file with a match, announced before its first one: the host's `0x01` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchFile {
    /// Which root: its position among the request's roots, from `0`.
    pub root: u64,
    /// The file's path below the root with `/` between names; empty when the root is the file.
    pub path: String,
    /// Its size in bytes.
    pub size: u64,
}

/// One matching line of the file last announced: the host's `0x02` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    /// The line's number, from `1`.
    pub line: u64,
    /// The line without its line ending, decoded as UTF-8 (invalid bytes replaced), cut at `[search] max_line_bytes`.
    pub text: String,
    /// The lines before it, oldest first, up to the request's `context`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    /// The lines after it, up to the request's `context`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// A line was cut at `max_line_bytes`.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub truncated: bool,
}

/// Something that could not be read: the host's `0x03` frame. The search goes on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchItemError {
    /// Which root.
    pub root: u64,
    /// The path below the root it concerns; absent for the root itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// What kind of failure.
    pub code: ErrorCode,
    /// A sentence for a person; it never carries a secret.
    #[serde(default)]
    pub message: String,
}

impl SearchItemError {
    /// The error as a plain body.
    pub fn body(&self) -> ErrorBody {
        ErrorBody::new(self.code, self.message.clone())
    }
}

/// What a search left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Skipped {
    /// Files with a NUL in their first `[search] binary_probe` bytes.
    #[serde(default)]
    pub binary: u64,
    /// Files over `[search] max_file_bytes`.
    #[serde(default)]
    pub large: u64,
    /// Files that could not be opened or read (each also sent as an item error).
    #[serde(default)]
    pub unreadable: u64,
}

/// The end of a search: the host's `0x04` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SearchDone {
    /// How many files were read.
    pub files: u64,
    /// How many matches were sent.
    pub matches: u64,
    /// What was left out.
    #[serde(default)]
    pub skipped: Skipped,
    /// The search stopped at a bound (matches, files or the deadline) before it was done.
    #[serde(default)]
    pub truncated: bool,
}

const FILE: u8 = 0x01;
const MATCH: u8 = 0x02;
const ITEM_ERROR: u8 = 0x03;
const DONE: u8 = 0x04;
const ERROR: u8 = 0x05;

fn tagged(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(body.len() + 1);
    frame.push(tag);
    frame.extend_from_slice(body);
    frame
}

/// A frame of a search, from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SearchOutput {
    /// A file with a match; its matches follow.
    File(SearchFile),
    /// A matching line.
    Match(SearchMatch),
    /// A directory or file could not be read, or a root is outside the peer's roots.
    ItemError(SearchItemError),
    /// The search ended; the last frame.
    Done(SearchDone),
    /// The search failed as a whole; the last frame.
    Error(ErrorBody),
}

impl SearchOutput {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::File(body) => tagged(FILE, &encode(body)?),
            Self::Match(body) => tagged(MATCH, &encode(body)?),
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
        let (tag, body) = frame.split_first().ok_or(Error::Short)?;
        match *tag {
            FILE => decode(body).map(Self::File),
            MATCH => decode(body).map(Self::Match),
            ITEM_ERROR => decode(body).map(Self::ItemError),
            DONE => decode(body).map(Self::Done),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}
