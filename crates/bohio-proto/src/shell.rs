//! Shell kinds, shared by the host's capabilities and explicit shell commands, and interactive
//! shell sessions on a terminal (`bohio-shell/1`).
//!
//! The open's metadata is a CBOR [`Request`]. Afterwards every stream message is one frame: a tag
//! byte, then the body. Terminal bytes travel raw behind their tag, never inside CBOR.
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | host to client | `0x01` | [`Output::Started`] | CBOR [`Started`] |
//! | host to client | `0x02` | [`Output::Data`] | raw bytes |
//! | host to client | `0x08` | [`Output::Signalled`] | CBOR [`Signalled`] |
//! | host to client | `0x04` | [`Output::Exit`] | CBOR [`Exit`] |
//! | host to client | `0x05` | [`Output::Error`] | CBOR [`ErrorBody`] |
//! | client to host | `0x10` | [`Input::Data`] | raw bytes |
//! | client to host | `0x11` | [`Input::Resize`] | CBOR [`Size`] |
//! | client to host | `0x12` | [`Input::Kill`] | empty |
//! | client to host | `0x15` | [`Input::Signal`] | CBOR [`SignalRequest`] |

use serde::{Deserialize, Serialize};

use crate::codec::{split, tagged};
use crate::error::{Error, ErrorBody};
use crate::exec::Env;
use crate::session::SessionRequest;
pub use crate::signal::{Signal, SignalRequest, Signalled};
use crate::{decode, encode};

/// A shell a host may offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShellKind {
    /// POSIX `sh`.
    Sh,
    /// GNU `bash`.
    Bash,
    /// `zsh`.
    Zsh,
    /// The Windows command interpreter, `cmd`.
    Cmd,
    /// Windows PowerShell, `powershell`.
    Powershell,
    /// PowerShell 7 and later, `pwsh`.
    Pwsh,
    /// A kind a newer peer knows.
    #[serde(other)]
    Other,
}

impl ShellKind {
    /// Every kind this version knows, in the order a host probes for them.
    pub const KNOWN: [Self; 6] = [
        Self::Sh,
        Self::Bash,
        Self::Zsh,
        Self::Cmd,
        Self::Powershell,
        Self::Pwsh,
    ];

    /// The program name looked up on the host's `PATH`; empty for [`ShellKind::Other`].
    pub const fn program(self) -> &'static str {
        match self {
            Self::Sh => "sh",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Cmd => "cmd",
            Self::Powershell => "powershell",
            Self::Pwsh => "pwsh",
            Self::Other => "",
        }
    }
}

/// The terminal size a session starts with when the request names none.
pub const DEFAULT_COLS: u16 = 80;
/// The terminal height a session starts with when the request names none.
pub const DEFAULT_ROWS: u16 = 24;
/// The largest width or height a terminal takes: Windows pseudo-consoles hold them as signed
/// 16-bit numbers.
pub const MAX_DIMENSION: u16 = 32_767;

const fn default_cols() -> u16 {
    DEFAULT_COLS
}

const fn default_rows() -> u16 {
    DEFAULT_ROWS
}

/// What to start on the terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Program {
    /// One of the shells the host's `info` lists, found on its `PATH`.
    Kind {
        /// Which shell.
        kind: ShellKind,
    },
    /// An explicit program and its arguments, started directly.
    Argv {
        /// The program, then its arguments.
        argv: Vec<String>,
    },
    /// The node's console: whatever its terminal source serves, with no program started. For the `native` source this
    /// is its default shell; for a serial line it is the line. A `Kind` or `Argv` request to a source that starts no
    /// program is `unsupported`.
    Console {
        /// The terminal source; the node's default one when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}

/// What a `bohio-shell/1` stream opens: the open's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The shell or program on the terminal.
    pub program: Program,
    /// The working directory, in the host's native text; the host's own when absent.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Changes to the environment the program starts with. `TERM` is `xterm-256color` unless set.
    #[serde(default)]
    pub env: Env,
    /// Terminal width in columns, 1 to [`MAX_DIMENSION`]. Default [`DEFAULT_COLS`].
    #[serde(default = "default_cols")]
    pub cols: u16,
    /// Terminal height in rows, 1 to [`MAX_DIMENSION`]. Default [`DEFAULT_ROWS`].
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Milliseconds after which the host ends the program's tree and reports `timed_out`; no deadline when absent.
    /// With `argv` this is `exec` on a terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Run as a detached session: a stream that closes detaches the program instead of ending it, and
    /// `bohio-attach/1` picks the terminal up again. Needs the peer's `session` permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionRequest>,
}

impl Request {
    /// A session on the shell `kind`, at the default size.
    pub fn kind(kind: ShellKind) -> Self {
        Self::new(Program::Kind { kind })
    }

    /// A session on `argv`, started directly, at the default size.
    pub fn argv<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new(Program::Argv {
            argv: argv.into_iter().map(Into::into).collect(),
        })
    }

    /// A request for the node's console of `source` (its default terminal source when `None`): no program is started.
    pub fn console(source: Option<&str>) -> Self {
        Self::new(Program::Console {
            source: source.map(str::to_owned),
        })
    }

    fn new(program: Program) -> Self {
        Self {
            program,
            cwd: None,
            env: Env::default(),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            timeout_ms: None,
            session: None,
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

/// The program is running on its terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Started {
    /// The host's process id of the shell; absent for a console, which starts no program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The session's id (128 bits as hex), when the request asked for a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl Started {
    /// A report for a program that is not a session.
    pub const fn new(pid: u32) -> Self {
        Self {
            pid: Some(pid),
            session: None,
        }
    }
}

/// How the session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Exit {
    /// The exit code, when the program ended by itself.
    pub code: Option<i32>,
    /// The signal that ended it, on Unix.
    pub signal: Option<i32>,
    /// The deadline passed and the host ended the tree.
    pub timed_out: bool,
    /// The host ended the tree because the client asked, closed the stream or the deadline passed.
    pub killed: bool,
}

/// A terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    /// Width in columns, 1 to [`MAX_DIMENSION`].
    pub cols: u16,
    /// Height in rows, 1 to [`MAX_DIMENSION`].
    pub rows: u16,
}

/// A frame from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Output {
    /// The program started; the first frame.
    Started(Started),
    /// Bytes the terminal produced: the program's output as a terminal renders it, escape
    /// sequences included.
    Data(Vec<u8>),
    /// The host's answer to a signal frame.
    Signalled(Signalled),
    /// The program ended; the last frame, after every output byte.
    Exit(Exit),
    /// The session could not start or failed; the last frame.
    Error(ErrorBody),
}

/// A frame from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Input {
    /// Bytes typed at the terminal; Enter is a carriage return.
    Data(Vec<u8>),
    /// The terminal changed size.
    Resize(Size),
    /// End the program's tree now.
    Kill,
    /// Deliver a signal to the program; the host answers with a `signalled` frame.
    Signal(Signal),
}

const STARTED: u8 = 0x01;
const DATA: u8 = 0x02;
const EXIT: u8 = 0x04;
const ERROR: u8 = 0x05;
const INPUT: u8 = 0x10;
const RESIZE: u8 = 0x11;
const KILL: u8 = 0x12;
const SIGNALLED: u8 = 0x08;
const SIGNAL: u8 = 0x15;

impl Output {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Started(body) => tagged(STARTED, &encode(body)?),
            Self::Data(bytes) => tagged(DATA, bytes),
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
            DATA => Ok(Self::Data(body.to_vec())),
            SIGNALLED => decode(body).map(Self::Signalled),
            EXIT => decode(body).map(Self::Exit),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}

impl Input {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Data(bytes) => tagged(INPUT, bytes),
            Self::Resize(size) => tagged(RESIZE, &encode(size)?),
            Self::Kill => tagged(KILL, &[]),
            Self::Signal(signal) => tagged(SIGNAL, &encode(&SignalRequest { signal: *signal })?),
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
            INPUT => Ok(Self::Data(body.to_vec())),
            RESIZE => decode(body).map(Self::Resize),
            KILL => Ok(Self::Kill),
            SIGNAL => decode::<SignalRequest>(body).map(|request| Self::Signal(request.signal)),
            other => Err(Error::Tag(other)),
        }
    }
}
