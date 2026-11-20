//! Hashing: the algorithm names, and the frames of `bohio-hash/1`.
//!
//! `bohio-hash/1`: the open's metadata is a [`HashRequest`]; the client then sends one
//! [`HashInput::Item`] per file and a [`HashInput::End`], and the host answers each item in order.
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | client to host | `0x10` | [`HashInput::Item`] | CBOR [`HashItem`] |
//! | client to host | `0x11` | [`HashInput::End`] | empty |
//! | host to client | `0x01` | [`HashOutput::Result`] | CBOR [`HashResult`] |
//! | host to client | `0x02` | [`HashOutput::Blocks`] | CBOR [`HashBlocks`] |
//! | host to client | `0x03` | [`HashOutput::ItemError`] | CBOR [`ItemError`] |
//! | host to client | `0x04` | [`HashOutput::Done`] | CBOR [`HashDone`] |
//! | host to client | `0x05` | [`HashOutput::Error`] | CBOR [`ErrorBody`] |

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::codec::bytes;
use crate::error::{Error, ErrorBody, ErrorCode};
use crate::fs::Timestamp;
use crate::{decode, encode};

/// A digest algorithm, by the name the wire uses.
///
/// Every digest is written as lowercase hexadecimal in the byte order the reference tool prints
/// (`md5sum`, `sha256sum`, `b3sum`, `xxhsum`): the bytes of the digest for the cryptographic ones,
/// the value in big-endian for `xxh3`, `xxh128` (high half first), `xxh64` and `crc32c`. A name this
/// version does not know is [`Algorithm::Other`], so a newer peer's list still decodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Algorithm {
    /// MD5, 128 bits. For comparing with existing sums; not collision resistant.
    Md5,
    /// SHA-1, 160 bits. For comparing with existing sums; not collision resistant.
    Sha1,
    /// SHA-256, 256 bits.
    Sha256,
    /// SHA-384, 384 bits.
    Sha384,
    /// SHA-512, 512 bits.
    Sha512,
    /// BLAKE3, 256 bits.
    Blake3,
    /// XXH3 with 64 bits. Not cryptographic.
    Xxh3,
    /// XXH3 with 128 bits. Not cryptographic.
    Xxh128,
    /// XXH64 with seed 0. Not cryptographic.
    Xxh64,
    /// CRC-32C (Castagnoli), 8 hex digits. Not cryptographic.
    Crc32c,
    /// A name this version does not know.
    Other(String),
}

impl Algorithm {
    /// Every algorithm this version names, in the order a listing shows them.
    pub const KNOWN: [Self; 10] = [
        Self::Md5,
        Self::Sha1,
        Self::Sha256,
        Self::Sha384,
        Self::Sha512,
        Self::Blake3,
        Self::Xxh3,
        Self::Xxh128,
        Self::Xxh64,
        Self::Crc32c,
    ];

    /// The name on the wire.
    pub fn name(&self) -> &str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
            Self::Blake3 => "blake3",
            Self::Xxh3 => "xxh3",
            Self::Xxh128 => "xxh128",
            Self::Xxh64 => "xxh64",
            Self::Crc32c => "crc32c",
            Self::Other(name) => name,
        }
    }

    /// The algorithm a name stands for; [`Algorithm::Other`] when this version does not know it.
    pub fn from_name(name: &str) -> Self {
        Self::KNOWN
            .into_iter()
            .find(|known| known.name() == name)
            .unwrap_or_else(|| Self::Other(name.to_owned()))
    }

    /// Whether this version names the algorithm (it is not [`Algorithm::Other`]).
    pub const fn is_known(&self) -> bool {
        !matches!(self, Self::Other(_))
    }

    /// How many hexadecimal digits a digest of this algorithm has; `None` for an unknown one.
    pub const fn hex_len(&self) -> Option<usize> {
        Some(match self {
            Self::Md5 | Self::Xxh128 => 32,
            Self::Sha1 => 40,
            Self::Sha256 | Self::Blake3 => 64,
            Self::Sha384 => 96,
            Self::Sha512 => 128,
            Self::Xxh3 | Self::Xxh64 => 16,
            Self::Crc32c => 8,
            Self::Other(_) => return None,
        })
    }
}

impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Serialize for Algorithm {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Algorithm {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|name| Self::from_name(&name))
    }
}

/// Digests by algorithm, each as lowercase hex.
pub type Digests = BTreeMap<Algorithm, String>;

/// The options of a `bohio-hash/1` stream: the open's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashRequest {
    /// The algorithms every item is hashed with, in one pass. At least one.
    #[serde(default)]
    pub algorithms: Vec<Algorithm>,
    /// Ask for block hashes of each item too.
    #[serde(default)]
    pub blocks: Option<BlockOptions>,
}

impl HashRequest {
    /// A request for `algorithms` of whole files.
    pub fn new(algorithms: impl IntoIterator<Item = Algorithm>) -> Self {
        Self {
            algorithms: algorithms.into_iter().collect(),
            blocks: None,
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

/// How block hashes are made. The host does not serve them yet: a request that sets this is
/// `unsupported`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlockOptions {
    /// The block size in bytes; the host chooses when absent.
    #[serde(default)]
    pub size: Option<u64>,
    /// The weak hash's name; the host chooses when absent.
    #[serde(default)]
    pub weak: Option<String>,
    /// How many bytes of each strong hash are kept; the host chooses when absent.
    #[serde(default)]
    pub strong_len: Option<u64>,
}

/// One file to hash: the client's `0x10` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashItem {
    /// The file.
    pub path: String,
    /// The first byte hashed. Default `0`.
    #[serde(default)]
    pub offset: u64,
    /// How many bytes at most; to the end of the file when absent.
    #[serde(default)]
    pub length: Option<u64>,
    /// With `if_modified`: when the file's size is this and its modification time is `if_modified`,
    /// the host reads nothing and answers `unchanged`.
    #[serde(default)]
    pub if_size: Option<u64>,
    /// See `if_size`.
    #[serde(default)]
    pub if_modified: Option<Timestamp>,
}

impl HashItem {
    /// The whole of `path`.
    pub fn whole(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            length: None,
            if_size: None,
            if_modified: None,
        }
    }
}

/// The digests of one item: the host's `0x01` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashResult {
    /// Which item this answers: its position among the client's items, from `0`.
    pub index: u64,
    /// The file's size when it was opened.
    pub size: u64,
    /// When the file's content last changed, when the host knows.
    #[serde(default)]
    pub modified: Option<Timestamp>,
    /// How many bytes were hashed (none when `unchanged`).
    pub length: u64,
    /// The item's `if_size` and `if_modified` both equal the file's: nothing was read, and
    /// `hashes` is empty.
    #[serde(default)]
    pub unchanged: bool,
    /// The digests of the bytes hashed.
    #[serde(default)]
    pub hashes: Digests,
}

/// Block hashes of one item: the host's `0x02` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashBlocks {
    /// Which item this belongs to.
    pub index: u64,
    /// The number of the first block of this batch, from `0`.
    pub first: u64,
    /// The block size in bytes.
    pub size: u64,
    /// The weak hash of each block; empty when the host sends none.
    #[serde(default)]
    pub weak: Vec<u32>,
    /// The strong hashes, joined: each block's is `strong_len` bytes.
    #[serde(with = "bytes", default)]
    pub strong: Vec<u8>,
}

/// An item that failed: the host's `0x03` frame. The stream goes on with the next item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemError {
    /// Which item failed.
    pub index: u64,
    /// What kind of failure.
    pub code: ErrorCode,
    /// A sentence for a person; it never carries a secret.
    #[serde(default)]
    pub message: String,
}

impl ItemError {
    /// The failure of item `index`.
    pub fn new(index: u64, body: ErrorBody) -> Self {
        Self {
            index,
            code: body.code,
            message: body.message,
        }
    }

    /// The error as a plain body.
    pub fn body(&self) -> ErrorBody {
        ErrorBody::new(self.code, self.message.clone())
    }
}

/// The end of a hash stream: the host's `0x04` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashDone {
    /// How many items the host answered, results and item errors together.
    pub items: u64,
}

const RESULT: u8 = 0x01;
const BLOCKS: u8 = 0x02;
const ITEM_ERROR: u8 = 0x03;
const DONE: u8 = 0x04;
const ERROR: u8 = 0x05;
const ITEM: u8 = 0x10;
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

/// A frame of a hash stream, from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HashOutput {
    /// The digests of one item.
    Result(HashResult),
    /// A batch of block hashes of one item.
    Blocks(HashBlocks),
    /// One item failed.
    ItemError(ItemError),
    /// Every item was answered; the last frame.
    Done(HashDone),
    /// The stream failed or was refused; the last frame.
    Error(ErrorBody),
}

impl HashOutput {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Result(body) => tagged(RESULT, &encode(body)?),
            Self::Blocks(body) => tagged(BLOCKS, &encode(body)?),
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
            RESULT => decode(body).map(Self::Result),
            BLOCKS => decode(body).map(Self::Blocks),
            ITEM_ERROR => decode(body).map(Self::ItemError),
            DONE => decode(body).map(Self::Done),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}

/// A frame of a hash stream, from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HashInput {
    /// Another file to hash.
    Item(HashItem),
    /// No more items.
    End,
}

impl HashInput {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the item cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Item(item) => tagged(ITEM, &encode(item)?),
            Self::End => tagged(END, &[]),
        })
    }

    /// Reads one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know,
    /// [`Error::Decode`] for an item that is not one.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = split(frame)?;
        match tag {
            ITEM => decode(body).map(Self::Item),
            END => Ok(Self::End),
            other => Err(Error::Tag(other)),
        }
    }
}
