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
//! | host to client | `0x03` | [`ReadOutput::ItemError`] | CBOR [`ItemError`] (a bundle) |
//! | host to client | `0x04` | [`ReadOutput::Done`] | CBOR [`ReadDone`] |
//! | host to client | `0x05` | [`ReadOutput::Error`] | CBOR [`ErrorBody`] |
//! | host to client | `0x06` | [`ReadOutput::Compressed`] | a piece of the compressed stream |
//! | host to client | `0x07` | [`ReadOutput::End`] | CBOR [`BundleEnd`] (a bundle) |
//! | host to client | `0x08` | [`ReadOutput::Progress`] | CBOR [`Progress`] |
//! | host to client | `0x09` | [`ReadOutput::Copy`] | CBOR [`DeltaCopy`] (a delta read) |
//! | client to host | `0x10` | [`ReadInput::Item`] | CBOR [`ReadItem`] (a bundle) |
//! | client to host | `0x11` | [`ReadInput::ItemsEnd`] | empty (a bundle) |
//! | client to host | `0x12` | [`ReadInput::Signature`] | CBOR [`SignatureBatch`] (a delta read) |
//! | client to host | `0x13` | [`ReadInput::SignatureEnd`] | empty (a delta read) |
//!
//! `bohio-write/1`: the open's metadata is a [`WriteRequest`].
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | host to client | `0x01` | [`WriteOutput::Ready`] | empty, or CBOR [`Ready`] |
//! | host to client | `0x02` | [`WriteOutput::Flushed`] | CBOR [`Flushed`] (answers a flush) |
//! | host to client | `0x03` | [`WriteOutput::ItemError`] | CBOR [`ItemError`] (a bundle) |
//! | host to client | `0x04` | [`WriteOutput::Done`] | CBOR [`WriteDone`] |
//! | host to client | `0x05` | [`WriteOutput::Error`] | CBOR [`ErrorBody`] |
//! | host to client | `0x07` | [`WriteOutput::End`] | CBOR [`BundleEnd`] (a bundle) |
//! | client to host | `0x10` | [`WriteInput::Data`] | raw bytes |
//! | client to host | `0x11` | [`WriteInput::End`] | empty (the end of the data, or of a bundle's item) |
//! | client to host | `0x12` | [`WriteInput::Flush`] | empty (`append` and `at`) |
//! | client to host | `0x14` | [`WriteInput::Compressed`] | a piece of the compressed stream |
//! | client to host | `0x15` | [`WriteInput::Copy`] | CBOR [`DeltaCopy`] (a delta write) |
//! | client to host | `0x16` | [`WriteInput::Item`] | CBOR [`WriteItem`] (a bundle) |
//! | client to host | `0x17` | [`WriteInput::ItemsEnd`] | empty (a bundle) |

use serde::{Deserialize, Serialize};

use crate::codec::bytes;
use crate::compress::{Compression, Progress, Resume};
use crate::error::{Error, ErrorBody};
use crate::hash::{Algorithm, Digests, ItemError};
use crate::{decode, encode};

/// The name of the weak checksum a delta uses: rsync's rolling sum of two 16-bit halves.
pub const WEAK_RSYNC: &str = "rsync";

/// Consecutive blocks of the old file a delta takes as they are: the host's `0x09` frame of a delta read
/// and the client's `0x15` frame of a delta write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaCopy {
    /// The first block, from `0`.
    pub block: u64,
    /// How many blocks, at least 1. The old file's last block may be shorter than the block size.
    pub count: u64,
}

/// How a read matches the client's old copy: the open's `delta` option. The client then sends its signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadDelta {
    /// The bytes in each block of the client's old copy but the last.
    pub block_size: u64,
    /// The weak checksum, `rsync`.
    pub weak: String,
    /// The strong hash of each block.
    pub strong: Algorithm,
    /// How many leading bytes of each strong hash the signature keeps.
    pub strong_len: u64,
    /// The size of the client's old copy: what tells a block from the shorter last one.
    pub base_size: u64,
}

/// One batch of the client's signature: the client's `0x12` frame of a delta read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureBatch {
    /// The number of the first block of this batch, from `0`; batches arrive in order.
    pub first: u64,
    /// The weak value of each block.
    #[serde(default)]
    pub weak: Vec<u32>,
    /// The strong hashes, joined: each block's is `strong_len` bytes.
    #[serde(with = "bytes", default)]
    pub strong: Vec<u8>,
}

/// What a delta write is made against: the file as the client saw it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeltaBase {
    /// The target's size.
    pub size: u64,
    /// The target's modification time, when the client knows it.
    #[serde(default)]
    pub modified: Option<Timestamp>,
    /// Digests of the target the host checks too, by algorithm (lowercase hex).
    #[serde(default)]
    pub hashes: Digests,
}

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
    /// The device the file lies on, with `inode` what makes two names one file; absent where the host cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<u64>,
    /// The file's number on its device; absent where the host cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inode: Option<u64>,
    /// How many names the file has; absent where the host cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<u64>,
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
    /// The file; empty for a bundle, whose files come as items.
    #[serde(default)]
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
    /// Send only what differs from the client's old copy, whose signature the client sends after the
    /// open. The host answers `copy` frames for the blocks the copy has and data for the rest. Needs
    /// `info.transfer.delta`; not with a prepared read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<ReadDelta>,
    /// A bundle: many files on this one stream. The client sends one `item` frame per file and an `items
    /// end`, and the host answers each in order (a header, the data, `done`, or an item error) and `end`.
    /// `hashes` and `compression` apply to every item, one compression context spanning them all. Not with
    /// `prepare`, `resume` or `delta`.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub items: bool,
    /// Follow the file as it grows (`tail -f`): the host sends what is there from the start, then keeps sending what is
    /// appended until the client closes the stream. Needs `length` absent, and no hashes, compression, `prepare`,
    /// `resume`, `delta` or `items`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow: Option<ReadFollow>,
}

/// How a followed read watches the file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReadFollow {
    /// What the follow keeps looking at. Default [`FollowBy::Handle`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<FollowBy>,
    /// How often the host looks for more bytes, in milliseconds; at least 1. The host's `[filesystem] follow_interval`
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
    /// Start this many bytes before the end of the file (`0`: at the end, so only what is appended comes), instead of at
    /// `offset`. A file shorter than that is sent from its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u64>,
}

/// What a follow keeps looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowBy {
    /// The file that was opened, even when it is renamed or removed.
    Handle,
    /// The path: when another file takes it (a log rotation) the host opens that one and sends it from its start.
    Name,
}

/// The file got shorter, or was replaced: the host's `0x0a` frame of a followed read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Truncated {
    /// The file's size now. After a shrink the host goes on from this end; after a replacement it sends the new file from
    /// its start, so the next bytes are the beginning of a file of at least this size.
    pub size: u64,
    /// The file was replaced by another (by name), not shortened.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub replaced: bool,
}

/// One file of a bundle read: the client's `0x10` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadItem {
    /// The file.
    pub path: String,
    /// The first byte to send. Default `0`.
    #[serde(default)]
    pub offset: u64,
    /// How many bytes at most; to the end of the file when absent.
    #[serde(default)]
    pub length: Option<u64>,
}

impl ReadItem {
    /// The whole of `path`.
    pub fn whole(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            length: None,
        }
    }
}

/// The end of a bundle: the host's `0x07` frame, once every item was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BundleEnd {
    /// How many items were answered.
    pub items: u64,
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
            delta: None,
            items: false,
            follow: None,
        }
    }

    /// A bundle: the files come as items.
    pub fn bundle() -> Self {
        Self {
            items: true,
            ..Self::whole("")
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
    /// The read is a delta: `copy` frames and data frames add up to `length` bytes.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub delta: bool,
    /// The read follows the file: `length` is what is sent up to its end when the read starts, more comes as the file
    /// grows, and no `done` frame ends it; the client closes the stream.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub follow: bool,
    /// In a bundle, which item this is: its position among the client's items, from `0`. The first
    /// frame of a bundle, which says what the host compresses with, has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u64>,
    /// In a bundle, the file's modification time when the host knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<Timestamp>,
    /// In a bundle, the file's Unix permission bits when it has them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
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
            delta: false,
            follow: false,
            index: None,
            modified: None,
            mode: None,
        }
    }
}

/// The last frame of a read that succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReadDone {
    /// The digests of the bytes sent that the request named.
    #[serde(default)]
    pub hashes: Digests,
    /// In a bundle, which item this ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u64>,
}

/// How a `bohio-write/1` stream puts the bytes in place.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
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
    /// Build the new file from the target and what the client sends: `copy` frames name blocks of the
    /// target, data frames carry the rest, and the result replaces the target as `replace` does. The
    /// target must be as `base` says, or the write is `changed`. Needs `fs.read` as well as `fs.write`.
    Delta {
        /// The block size the client's copies are counted in.
        block_size: u64,
        /// The target as the client saw it.
        base: DeltaBase,
    },
    /// Continue a `replace` whose stream ended early (it asked for `resume`): the host answers `ready` with
    /// the offset it holds, the client sends the file from there, and the new file replaces the target as
    /// `replace` does. Only the peer that began it, for the same path, may continue it.
    Resume {
        /// The token the first `ready` named.
        token: String,
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
    /// The digests (lowercase hex) the data must have, by algorithm; only for `replace`, `create` and
    /// `delta` (of the whole new file): after the data ends each is checked, and on a mismatch nothing is put in place.
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
    /// A bundle: many files on this one stream. The client sends, per file, an `item` frame, its data and
    /// an `end`, then `items end`; the host answers each file with `done` or an item error, then `end`.
    /// The path, mode, parents, file mode and `expect` come from each item. `hashes` and `compression` apply
    /// to every item, one compression context spanning them all. Each file is put in place on its own.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub items: bool,
    /// For `replace`: keep the temporary file when the stream ends early (for `[filesystem] resume_ttl`), and
    /// answer `ready` with a token to continue it by. `unsupported` where the host keeps none.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub resume: bool,
    /// For `replace`: the target as the client saw it. The write is `changed` when the target's size, modification time
    /// or digests differ, before `ready` and again just before the rename, and replaces nothing. The target must exist
    /// (`not_found`). Needs `fs.read` as well as `fs.write`, because a digest that matches or not tells what the file holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<DeltaBase>,
}

/// One file of a bundle write: the client's `0x16` frame. The data follows, then an `end`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WriteItem {
    /// The file.
    pub path: String,
    /// How the bytes are put in place: `replace` (the default) or `create`.
    #[serde(default)]
    pub mode: WriteMode,
    /// Create missing directories above the file.
    #[serde(default)]
    pub parents: bool,
    /// Unix permission bits for a file this item creates.
    #[serde(default)]
    pub file_mode: Option<u32>,
    /// The modification time the file gets before it is put in place, so a sync's next quick check by
    /// size and time finds it unchanged.
    #[serde(default)]
    pub modified: Option<Timestamp>,
    /// The digests the data must have, as a single write's `expect`.
    #[serde(default)]
    pub expect: Digests,
}

impl WriteItem {
    /// An item that replaces `path`.
    pub fn replace(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }
}

/// The body of a `ready` that has something to say: the host's `0x01` frame. A `ready` with no body says nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Ready {
    /// For a write that asked to `resume`: the token that continues it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// For a write that continues one: how many bytes of the file the host holds, where the client goes on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

/// What a flush answers: the host's `0x02` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Flushed {
    /// The bytes this stream has written to the file, all of them now handed to the operating system (and
    /// to storage when the host syncs writes).
    pub written: u64,
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
            items: false,
            resume: false,
            base: None,
        }
    }

    /// A bundle: the files come as items.
    pub fn bundle() -> Self {
        Self {
            items: true,
            ..Self::replace("")
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
    /// In a bundle, which item this ends: its position among the client's items, from `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u64>,
}

const HEADER: u8 = 0x01;
const READY: u8 = 0x01;
const DATA: u8 = 0x02;
const FLUSHED: u8 = 0x02;
const ITEM_ERROR: u8 = 0x03;
const DONE: u8 = 0x04;
const ERROR: u8 = 0x05;
const COMPRESSED: u8 = 0x06;
const END: u8 = 0x07;
const PROGRESS: u8 = 0x08;
const COPY: u8 = 0x09;
const TRUNCATED: u8 = 0x0a;
const WRITE_DATA: u8 = 0x10;
const WRITE_END: u8 = 0x11;
const WRITE_FLUSH: u8 = 0x12;
const WRITE_COMPRESSED: u8 = 0x14;
const WRITE_COPY: u8 = 0x15;
const WRITE_ITEM: u8 = 0x16;
const WRITE_ITEMS_END: u8 = 0x17;
const READ_ITEM: u8 = 0x10;
const READ_ITEMS_END: u8 = 0x11;
const SIGNATURE: u8 = 0x12;
const SIGNATURE_END: u8 = 0x13;

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
    /// The file was opened; the first frame. A bundle's first frame says what the host compresses with, and
    /// each item then starts with one of its own.
    Header(ReadHeader),
    /// Bytes of the file, in order.
    Data(Vec<u8>),
    /// A piece of the compressed stream. A `data` frame ends the compression context (see
    /// `docs/protocol.md`).
    Compressed(Vec<u8>),
    /// The host is still compressing a prepared read.
    Progress(Progress),
    /// A delta read: these blocks of the client's old copy stand here.
    Copy(DeltaCopy),
    /// A followed read: the file got shorter or was replaced.
    Truncated(Truncated),
    /// A bundle's item could not be read; the stream goes on with the next.
    ItemError(ItemError),
    /// Every byte was sent; the last frame (in a bundle, the end of one item).
    Done(ReadDone),
    /// Every item of a bundle was answered; the last frame.
    End(BundleEnd),
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
            Self::Copy(body) => tagged(COPY, &encode(body)?),
            Self::Truncated(body) => tagged(TRUNCATED, &encode(body)?),
            Self::ItemError(body) => tagged(ITEM_ERROR, &encode(body)?),
            Self::Done(body) => tagged(DONE, &encode(body)?),
            Self::End(body) => tagged(END, &encode(body)?),
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
            COPY => decode(body).map(Self::Copy),
            TRUNCATED => decode(body).map(Self::Truncated),
            ITEM_ERROR => decode(body).map(Self::ItemError),
            DONE => decode(body).map(Self::Done),
            END => decode(body).map(Self::End),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}

/// A frame of a read, from the client: a bundle's items, and a delta read's signature.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadInput {
    /// The next file of a bundle.
    Item(ReadItem),
    /// The bundle's last item was sent.
    ItemsEnd,
    /// The next blocks of the signature of the client's old copy.
    Signature(SignatureBatch),
    /// The signature is complete.
    SignatureEnd,
}

impl ReadInput {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Item(body) => tagged(READ_ITEM, &encode(body)?),
            Self::ItemsEnd => tagged(READ_ITEMS_END, &[]),
            Self::Signature(body) => tagged(SIGNATURE, &encode(body)?),
            Self::SignatureEnd => tagged(SIGNATURE_END, &[]),
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
            READ_ITEM => decode(body).map(Self::Item),
            READ_ITEMS_END => Ok(Self::ItemsEnd),
            SIGNATURE => decode(body).map(Self::Signature),
            SIGNATURE_END => Ok(Self::SignatureEnd),
            other => Err(Error::Tag(other)),
        }
    }
}

/// A frame of a write, from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteOutput {
    /// The target is open: the client may send data. The first frame.
    Ready(Ready),
    /// A flush was done: the answer to the client's `flush`.
    Flushed(Flushed),
    /// A bundle's item failed and nothing of it was put in place; the stream goes on with the next.
    ItemError(ItemError),
    /// The data is in place; the last frame (in a bundle, the end of one item).
    Done(WriteDone),
    /// Every item of a bundle was answered; the last frame.
    End(BundleEnd),
    /// The write failed and nothing was put in place (for `replace` and `create`); the last frame.
    Error(ErrorBody),
}

impl WriteOutput {
    /// A `ready` that says nothing.
    pub fn ready() -> Self {
        Self::Ready(Ready::default())
    }

    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Ready(body) if *body == Ready::default() => tagged(READY, &[]),
            Self::Ready(body) => tagged(READY, &encode(body)?),
            Self::Flushed(body) => tagged(FLUSHED, &encode(body)?),
            Self::ItemError(body) => tagged(ITEM_ERROR, &encode(body)?),
            Self::Done(body) => tagged(DONE, &encode(body)?),
            Self::End(body) => tagged(END, &encode(body)?),
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
            READY if body.is_empty() => Ok(Self::ready()),
            READY => decode(body).map(Self::Ready),
            FLUSHED => decode(body).map(Self::Flushed),
            ITEM_ERROR => decode(body).map(Self::ItemError),
            DONE => decode(body).map(Self::Done),
            END => decode(body).map(Self::End),
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
    /// A delta write: these blocks of the target stand here. It neither ends nor disturbs a compression
    /// context, since every compressed piece decodes completely.
    Copy(DeltaCopy),
    /// The next file of a bundle; its data follows.
    Item(WriteItem),
    /// Make what was written so far safe and say how much it was (`append` and `at` only).
    Flush,
    /// No more data: put it in place. In a bundle, the end of the item's data.
    End,
    /// The bundle's last item was sent.
    ItemsEnd,
}

impl WriteInput {
    /// The frame as one stream message.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Data(bytes) => tagged(WRITE_DATA, bytes),
            Self::Compressed(bytes) => tagged(WRITE_COMPRESSED, bytes),
            Self::Copy(body) => tagged(WRITE_COPY, &encode(body).unwrap_or_default()),
            Self::Item(body) => tagged(WRITE_ITEM, &encode(body).unwrap_or_default()),
            Self::Flush => tagged(WRITE_FLUSH, &[]),
            Self::End => tagged(WRITE_END, &[]),
            Self::ItemsEnd => tagged(WRITE_ITEMS_END, &[]),
        }
    }

    /// Reads one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know,
    /// [`Error::Decode`] for a copy or an item that is not one.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = split(frame)?;
        match tag {
            WRITE_DATA => Ok(Self::Data(body.to_vec())),
            WRITE_COMPRESSED => Ok(Self::Compressed(body.to_vec())),
            WRITE_COPY => decode(body).map(Self::Copy),
            WRITE_ITEM => decode(body).map(Self::Item),
            WRITE_FLUSH => Ok(Self::Flush),
            WRITE_END => Ok(Self::End),
            WRITE_ITEMS_END => Ok(Self::ItemsEnd),
            other => Err(Error::Tag(other)),
        }
    }
}
