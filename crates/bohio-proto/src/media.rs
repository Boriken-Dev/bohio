//! `bohio-media/1`: virtual media (an image the system boots or installs from), through a media source.
//!
//! One request a stream: the open's metadata is a [`Request`], and the host answers one [`Answer`].

use serde::{Deserialize, Serialize};

use crate::codec::{decode, encode};
use crate::error::{Error, ErrorBody};

/// What to do with the media.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Op {
    /// Lists the slots; changes nothing.
    Status,
    /// Inserts an image.
    Insert,
    /// Ejects what a slot holds.
    Eject,
}

impl Op {
    /// The word a command and an event use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Insert => "insert",
            Self::Eject => "eject",
        }
    }
}

/// Where an image comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Image {
    /// A URL the source fetches itself (a BMC does).
    Url {
        /// The URL.
        url: String,
    },
    /// A file of the node: it must lie under the peer's `fs_roots` (and the peer needs `fs.read`); the source reads it
    /// through the host, block by block.
    Path {
        /// The path on the node.
        path: String,
    },
}

/// The open's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The media source; the node's default one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// What to do.
    pub op: Op,
    /// The slot (`cd`, `floppy`...), for an insert or an eject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// The image, for an insert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Image>,
}

impl Request {
    /// A request for `op` of the default source, naming nothing else.
    #[must_use]
    pub const fn new(op: Op) -> Self {
        Self {
            source: None,
            op,
            slot: None,
            image: None,
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

/// One slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    /// Its name.
    pub slot: String,
    /// Whether an image is inserted.
    #[serde(default)]
    pub inserted: bool,
    /// What is inserted, for a person; never a URL's query.
    #[serde(default)]
    pub image: Option<String>,
    /// The image's size in bytes, when known.
    #[serde(default)]
    pub size: Option<u64>,
}

/// The slots after the operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slots {
    /// Every slot, or the one the operation changed.
    pub slots: Vec<Slot>,
}

/// The host's one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The slots.
    Slots(Slots),
    /// The request was refused or failed.
    Error(ErrorBody),
}

const SLOTS: u8 = 0x01;
const ERROR: u8 = 0x05;

impl Answer {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let (tag, body) = match self {
            Self::Slots(body) => (SLOTS, encode(body)?),
            Self::Error(body) => (ERROR, encode(body)?),
        };
        let mut frame = Vec::with_capacity(body.len() + 1);
        frame.push(tag);
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    /// Reads the answer.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know, [`Error::Decode`] for a
    /// body that is not what its tag says.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = frame.split_first().ok_or(Error::Short)?;
        match *tag {
            SLOTS => decode(body).map(Self::Slots),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}
