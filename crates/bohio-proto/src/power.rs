//! `bohio-power/1`: the power of the system a node is for, through a power source.
//!
//! One request a stream: the open's metadata is a [`Request`], and the host answers one [`Answer`]. A status asks the
//! source; every other action changes the system's power and is answered when the source says it finished, or with
//! `timeout` when `[power] action_timeout` passed first.

use serde::{Deserialize, Serialize};

use crate::codec::{decode, encode};
use crate::error::{Error, ErrorBody};

/// What a peer asks of the system's power.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Action {
    /// Reads the state; changes nothing.
    Status,
    /// Powers on.
    On,
    /// Powers off at once.
    Off,
    /// Asks the system to shut down.
    SoftOff,
    /// Powers off and on.
    Cycle,
    /// Resets without cutting power.
    Reset,
    /// A non-maskable interrupt.
    Nmi,
}

impl Action {
    /// Every action, in the order a command lists them.
    pub const ALL: [Self; 7] = [
        Self::Status,
        Self::On,
        Self::Off,
        Self::SoftOff,
        Self::Cycle,
        Self::Reset,
        Self::Nmi,
    ];

    /// The word a source's vocabulary and a command use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::On => "on",
            Self::Off => "off",
            Self::SoftOff => "soft_off",
            Self::Cycle => "cycle",
            Self::Reset => "reset",
            Self::Nmi => "nmi",
        }
    }
}

/// The open's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The power source; the node's default one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// What to do.
    pub action: Action,
}

impl Request {
    /// A request for `action` of the default source.
    #[must_use]
    pub const fn new(action: Action) -> Self {
        Self {
            source: None,
            action,
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

/// The system's power as the source reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// `on`, `off` or `unknown`.
    pub state: String,
    /// The state is the one the action asked for; false while a transition is under way.
    #[serde(default)]
    pub done: bool,
}

/// The host's one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The state after the action.
    State(State),
    /// The request was refused or failed.
    Error(ErrorBody),
}

const STATE: u8 = 0x01;
const ERROR: u8 = 0x05;

impl Answer {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let (tag, body) = match self {
            Self::State(body) => (STATE, encode(body)?),
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
            STATE => decode(body).map(Self::State),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}
