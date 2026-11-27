//! One-shot process execution on a `bohio-exec/1` stream.
//!
//! The open's metadata is a CBOR [`Request`]. Afterwards every stream message is one frame: a tag
//! byte, then the body. Output bytes travel raw behind their tag, never inside CBOR.
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | host to client | `0x01` | [`Output::Started`] | CBOR [`Started`] |
//! | host to client | `0x02` | [`Output::Stdout`] | raw bytes |
//! | host to client | `0x03` | [`Output::Stderr`] | raw bytes |
//! | host to client | `0x04` | [`Output::Exit`] | CBOR [`Exit`] |
//! | host to client | `0x05` | [`Output::Error`] | CBOR [`ErrorBody`] |
//! | host to client | `0x06` | [`Output::StdoutCompressed`] | a piece of the compressed standard output |
//! | host to client | `0x07` | [`Output::StderrCompressed`] | a piece of the compressed standard error |
//! | host to client | `0x08` | [`Output::Signalled`] | CBOR [`Signalled`] |
//! | client to host | `0x10` | [`Input::Stdin`] | raw bytes |
//! | client to host | `0x11` | [`Input::StdinEnd`] | empty |
//! | client to host | `0x12` | [`Input::Kill`] | empty |
//! | client to host | `0x13` | [`Input::StdinCompressed`] | a piece of the compressed standard input |
//! | client to host | `0x15` | [`Input::Signal`] | CBOR [`SignalRequest`] |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::codec::{split, tagged};
use crate::compress::Compression;
use crate::error::{Error, ErrorBody};
use crate::session::SessionRequest;
use crate::shell::ShellKind;
pub use crate::signal::{Signal, SignalRequest, Signalled};
use crate::{decode, encode};

/// What to run and how: the open's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The program, or an explicit shell script.
    pub command: Command,
    /// The working directory, in the host's native text; the host's own when absent.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Changes to the environment the process starts with.
    #[serde(default)]
    pub env: Env,
    /// Whether the client sends standard input.
    #[serde(default)]
    pub stdin: Stdin,
    /// Milliseconds after which the host ends the process tree; no deadline when absent.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// How the host sends standard output and standard error (each its own compression context);
    /// absent or `none` is raw frames. `started` says what it used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_compression: Option<Compression>,
    /// How the client sends standard input: a concrete algorithm (never `auto`) the host offers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_compression: Option<Compression>,
    /// Run as a detached session: a stream that closes detaches the process instead of ending it, and
    /// `bohio-attach/1` picks its output up again. Needs the peer's `session` permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionRequest>,
    /// The exec source that runs it; the node's default one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Request {
    /// A request to run `argv` directly: its first element is the program, never a shell.
    pub fn argv<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            command: Command::Argv {
                argv: argv.into_iter().map(Into::into).collect(),
            },
            cwd: None,
            env: Env::default(),
            stdin: Stdin::default(),
            timeout_ms: None,
            output_compression: None,
            stdin_compression: None,
            session: None,
            source: None,
        }
    }

    /// A request to run `script` in the shell `kind`, which the caller chose explicitly.
    pub fn shell(kind: ShellKind, script: impl Into<String>) -> Self {
        Self {
            command: Command::Shell {
                kind,
                script: script.into(),
            },
            ..Self::argv(core::iter::empty::<String>())
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

/// What is run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Command {
    /// A program and its arguments, started directly; no shell interprets them.
    Argv {
        /// The program, then its arguments.
        argv: Vec<String>,
    },
    /// A script handed to a named shell.
    Shell {
        /// Which shell.
        kind: ShellKind,
        /// The text the shell runs.
        script: String,
    },
}

/// Changes to the environment a process starts with, applied in this order: `clear`, `set`, `remove`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Env {
    /// Start from an empty environment instead of the host's.
    pub clear: bool,
    /// Variables to set.
    pub set: BTreeMap<String, String>,
    /// Variables to remove.
    pub remove: Vec<String>,
}

/// Whether the process reads standard input from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Stdin {
    /// The process reads nothing: its standard input is empty.
    #[default]
    Closed,
    /// The client sends [`Input::Stdin`] frames and ends them with [`Input::StdinEnd`].
    Open,
}

/// The process started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Started {
    /// The host's process id.
    pub pid: u32,
    /// What the host uses for output, never `auto`; absent when output is raw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_compression: Option<Compression>,
    /// What the host accepts for standard input; absent when none was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_compression: Option<Compression>,
    /// The session's id (128 bits as hex), when the request asked for a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl Started {
    /// A report for a process whose streams are raw.
    pub const fn new(pid: u32) -> Self {
        Self {
            pid,
            output_compression: None,
            stdin_compression: None,
            session: None,
        }
    }
}

/// How the process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Exit {
    /// The exit code, when it ended by itself.
    pub code: Option<i32>,
    /// The signal that ended it, on Unix.
    pub signal: Option<i32>,
    /// The deadline passed and the host ended the tree.
    pub timed_out: bool,
    /// The host ended the tree because the client asked, closed the stream or the deadline passed.
    pub killed: bool,
}

/// A frame from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Output {
    /// The process started.
    Started(Started),
    /// Bytes the process wrote to standard output.
    Stdout(Vec<u8>),
    /// Bytes the process wrote to standard error.
    Stderr(Vec<u8>),
    /// A piece of the compressed standard output. A `stdout` frame ends its compression context.
    StdoutCompressed(Vec<u8>),
    /// A piece of the compressed standard error. A `stderr` frame ends its compression context.
    StderrCompressed(Vec<u8>),
    /// The host's answer to a signal frame.
    Signalled(Signalled),
    /// The process ended; the last frame, after every output byte.
    Exit(Exit),
    /// The request failed before or instead of running; the last frame.
    Error(ErrorBody),
}

const STARTED: u8 = 0x01;
const STDOUT: u8 = 0x02;
const STDERR: u8 = 0x03;
const EXIT: u8 = 0x04;
const ERROR: u8 = 0x05;
const STDIN: u8 = 0x10;
const STDIN_END: u8 = 0x11;
const KILL: u8 = 0x12;
const STDOUT_COMPRESSED: u8 = 0x06;
const STDERR_COMPRESSED: u8 = 0x07;
const SIGNALLED: u8 = 0x08;
const SIGNAL: u8 = 0x15;
const STDIN_COMPRESSED: u8 = 0x13;

impl Output {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Started(body) => tagged(STARTED, &encode(body)?),
            Self::Stdout(bytes) => tagged(STDOUT, bytes),
            Self::Stderr(bytes) => tagged(STDERR, bytes),
            Self::StdoutCompressed(bytes) => tagged(STDOUT_COMPRESSED, bytes),
            Self::StderrCompressed(bytes) => tagged(STDERR_COMPRESSED, bytes),
            Self::Signalled(body) => tagged(SIGNALLED, &encode(body)?),
            Self::Exit(body) => tagged(EXIT, &encode(body)?),
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
            STARTED => decode(body).map(Self::Started),
            STDOUT => Ok(Self::Stdout(body.to_vec())),
            STDERR => Ok(Self::Stderr(body.to_vec())),
            STDOUT_COMPRESSED => Ok(Self::StdoutCompressed(body.to_vec())),
            STDERR_COMPRESSED => Ok(Self::StderrCompressed(body.to_vec())),
            SIGNALLED => decode(body).map(Self::Signalled),
            EXIT => decode(body).map(Self::Exit),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}

/// A frame from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Input {
    /// Bytes for the process's standard input.
    Stdin(Vec<u8>),
    /// A piece of the compressed standard input. A `stdin` frame ends its compression context.
    StdinCompressed(Vec<u8>),
    /// No more standard input.
    StdinEnd,
    /// End the process tree now.
    Kill,
    /// Deliver a signal to the process; the host answers with a `signalled` frame.
    Signal(Signal),
}

impl Input {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Stdin(bytes) => tagged(STDIN, bytes),
            Self::StdinCompressed(bytes) => tagged(STDIN_COMPRESSED, bytes),
            Self::StdinEnd => tagged(STDIN_END, &[]),
            Self::Kill => tagged(KILL, &[]),
            Self::Signal(signal) => tagged(SIGNAL, &encode(&SignalRequest { signal: *signal })?),
        })
    }

    /// Reads one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know,
    /// [`Error::Decode`] for a signal body that is not one.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = split(frame)?;
        match tag {
            SIGNAL => decode::<SignalRequest>(body).map(|request| Self::Signal(request.signal)),
            STDIN => Ok(Self::Stdin(body.to_vec())),
            STDIN_COMPRESSED => Ok(Self::StdinCompressed(body.to_vec())),
            STDIN_END => Ok(Self::StdinEnd),
            KILL => Ok(Self::Kill),
            other => Err(Error::Tag(other)),
        }
    }
}
