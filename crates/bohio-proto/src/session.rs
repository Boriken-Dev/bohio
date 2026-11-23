//! Detached sessions: a program that keeps running when its stream ends, and `bohio-attach/1`, which
//! picks its output up again.
//!
//! A session is opt-in twice: the peer holds the `session` permission, and the request of an exec
//! or a shell carries a [`SessionRequest`]. The host then starts the program as it always does, but
//! the stream closing only *detaches* it. Output is kept in memory, one byte offset across standard
//! output and standard error in the order they were written; `bohio-attach/1` asks for it from an
//! offset on.
//!
//! The open's metadata of `bohio-attach/1` is a CBOR [`Attach`]. The host's first frame is `0x09`
//! [`Attached`] (or `0x05` with an error); after it the stream is the exec's or the shell's own
//! frames (`kind` says which), from `from` on and then live, and the client sends that protocol's
//! own client frames.
//!
//! The control calls `sessions` and `end_session` ([`crate::control::Call`]) list and end sessions.

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::exec::Exit;
use crate::fs::Timestamp;
use crate::signal::Signal;
use crate::{decode, encode};

/// What to do when the kept output is full and the program writes more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OnFull {
    /// Forget the oldest output; an attach from before what is kept reports it as `lost`.
    DropOldest,
    /// Stop reading the program's output until a client has taken what is kept, so the program
    /// waits and nothing is lost. Output a client already received may still be forgotten.
    Pause,
}

/// The `session` of an exec or shell request: detach rather than end when the stream closes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionRequest {
    /// A name for listings; the host does not require it to be unique.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The bytes of output to keep, up to the host's `max_scrollback`; the host's default when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<u64>,
    /// What to do when the kept output is full; `pause` for an exec and `drop_oldest` for a shell when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_full: Option<OnFull>,
    /// Milliseconds a detached session nobody attaches to is kept before it is ended; the host's
    /// `linger` when absent (a request may only ask for less).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linger_ms: Option<u64>,
    /// Start detached: the host answers `started` and ends the stream, sending no output on it, so nothing is sent to
    /// a stream that nobody reads and the first `bohio-attach/1` reads everything from offset 0. Without it the stream
    /// that started the session is attached to it until it ends.
    #[serde(skip_serializing_if = "core::ops::Not::not")]
    pub detach: bool,
}

/// Which protocol a session speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Kind {
    /// A `bohio-exec/1` process: standard output and standard error apart.
    Exec,
    /// A `bohio-shell/1` program on a terminal.
    Shell,
    /// A kind this version does not know.
    #[serde(other)]
    Other,
}

/// The open's metadata of `bohio-attach/1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attach {
    /// The session's id, from `started` or a listing.
    pub session: String,
    /// The offset to read from: `0` for everything still kept; absent for only what comes next.
    #[serde(default)]
    pub from: Option<u64>,
}

impl Attach {
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
    /// [`Error::Decode`] when the bytes are not an attach request.
    pub fn from_metadata(metadata: &[u8]) -> Result<Self, Error> {
        decode(metadata)
    }
}

/// The host's first frame on an attached stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attached {
    /// Which protocol's frames follow.
    pub kind: Kind,
    /// The offset the output starts at: the one asked for, or the oldest still kept when it was older.
    pub from: u64,
    /// Bytes between the offset asked for and `from` that were forgotten (`0` when none).
    pub lost: u64,
}

const ATTACHED: u8 = 0x09;

impl Attached {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let body = encode(self)?;
        let mut frame = Vec::with_capacity(body.len() + 1);
        frame.push(ATTACHED);
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    /// Reads the frame.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for another frame, [`Error::Decode`] for a body that
    /// is not an `attached`.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        match frame.split_first() {
            None => Err(Error::Short),
            Some((&ATTACHED, body)) => decode(body),
            Some((&tag, _)) => Err(Error::Tag(tag)),
        }
    }
}

/// One session as `sessions` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    /// The id.
    pub id: String,
    /// The name the request gave.
    #[serde(default)]
    pub name: Option<String>,
    /// The protocol.
    pub kind: Kind,
    /// The program's name as asked for (its first word, never its arguments).
    pub program: String,
    /// When it started.
    pub started: Timestamp,
    /// Whether a stream is attached now.
    pub attached: bool,
    /// How it ended, once it has.
    #[serde(default)]
    pub exit: Option<Exit>,
    /// The offset the next byte of output will have.
    pub next: u64,
    /// The bytes kept now.
    pub buffered: u64,
    /// The owner, in a listing an `admin` asked for; absent in a peer's own.
    #[serde(default)]
    pub peer: Option<String>,
}

/// The signal `end_session` sends, and its default.
pub const END_DEFAULT: Signal = Signal::Kill;
