//! Control calls on a `bohio/1` stream: request and response with ids, so calls overlap.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{ErrorBody, ErrorCode};
use crate::fs::{Listing, Metadata, Timestamp};
use crate::info::{Info, PROTOCOL_VERSION, TransferInfo};
use crate::session::Listed;
use crate::signal::Signal;

fn default_version() -> u32 {
    PROTOCOL_VERSION
}

/// A call from the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The files source that serves it (see `info.access.fs`); the node's default one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Chosen by the client, unique on the stream; the response repeats it.
    pub id: u64,
    /// What is asked.
    pub call: Call,
}

/// What a request asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Call {
    /// Announces the client and the highest protocol version it speaks.
    Hello {
        /// The client's protocol version; [`PROTOCOL_VERSION`] when the key is missing.
        #[serde(default = "default_version")]
        version: u32,
        /// A name for logs; never a credential.
        #[serde(default)]
        client: String,
    },
    /// Asks for the host's [`Info`].
    Info,
    /// Metadata of a path; a symbolic link is described itself unless `follow`.
    Stat {
        /// The path.
        path: String,
        /// Describe what a final symbolic link points at.
        #[serde(default)]
        follow: bool,
    },
    /// One page of a directory's entries.
    List {
        /// The directory.
        path: String,
        /// Only names after this one (the previous page's `next`).
        #[serde(default)]
        after: Option<String>,
        /// The most entries wanted; the host's page size bounds it.
        #[serde(default)]
        limit: Option<u64>,
    },
    /// Creates a directory.
    Mkdir {
        /// The directory.
        path: String,
        /// Create missing parents; an existing directory is then not an error.
        #[serde(default)]
        parents: bool,
        /// Unix permission bits for what is created.
        #[serde(default)]
        mode: Option<u32>,
    },
    /// Removes a file, a symbolic link or a directory (not what a link points at).
    Remove {
        /// The path.
        path: String,
        /// Remove a directory and everything in it.
        #[serde(default)]
        recursive: bool,
    },
    /// Moves or renames within the host.
    Rename {
        /// The existing path.
        from: String,
        /// The new path.
        to: String,
        /// Replace an existing target; otherwise `exists`.
        #[serde(default)]
        overwrite: bool,
    },
    /// Copies a regular file, or with `recursive` a directory and everything in it (links are copied as
    /// links), on the host and never through the client.
    Copy {
        /// The file.
        from: String,
        /// The copy.
        to: String,
        /// Replace an existing target; otherwise `exists`. A directory is merged into an existing one.
        #[serde(default)]
        overwrite: bool,
        /// Copy a directory with what it holds; without it a directory is `bad_request`.
        #[serde(default, skip_serializing_if = "core::ops::Not::not")]
        recursive: bool,
        /// Give the copy the original's permission bits (without the set-ID bits) and modification time.
        #[serde(default, skip_serializing_if = "core::ops::Not::not")]
        preserve: bool,
    },
    /// Moves a file, a link or a directory: a rename where the host can, else (the two names lie on different
    /// filesystems) a copy that keeps permission bits and modification time, then removal of the original, all
    /// on the host. A failed copy leaves the original where it was.
    Move {
        /// The existing path.
        from: String,
        /// The new path.
        to: String,
        /// Replace an existing target; otherwise `exists`, which the host decides atomically where the
        /// operating system allows.
        #[serde(default)]
        overwrite: bool,
    },
    /// Sets a regular file's length: a shorter one cuts it, a longer one extends it with zeros.
    Truncate {
        /// The file.
        path: String,
        /// The new length in bytes.
        length: u64,
    },
    /// Creates a symbolic link at `link` holding the relative text `target`.
    Symlink {
        /// What the link names, relative to the link's directory; never leaving the roots.
        target: String,
        /// The link's path.
        link: String,
    },
    /// Reads a symbolic link's text.
    Readlink {
        /// The link.
        path: String,
    },
    /// Sets access and modification times; an absent one is left alone.
    SetTimes {
        /// The path.
        path: String,
        /// The new access time.
        #[serde(default)]
        accessed: Option<Timestamp>,
        /// The new modification time.
        #[serde(default)]
        modified: Option<Timestamp>,
    },
    /// Sets Unix permission bits; `unsupported` on a host without them.
    SetMode {
        /// The path.
        path: String,
        /// The bits, `0o7777` at most.
        mode: u32,
    },
    /// Sets the Windows read-only attribute; `unsupported` elsewhere.
    SetAttributes {
        /// The path.
        path: String,
        /// Whether the file is read-only; absent leaves it.
        #[serde(default)]
        readonly: Option<bool>,
    },
    /// What the host is running on: its name, uptime, processors, load, memory and disks. Needs `info`.
    Stats,
    /// Where each named program is on the host's own `PATH`, as an `exec` of a bare name finds it. Needs `info`.
    Which {
        /// The program names to look up; at least one.
        names: Vec<String>,
    },
    /// Lists the caller's detached sessions; an `admin` lists every peer's.
    Sessions,
    /// Ends a detached session with a signal (`kill` when none), and with it the program's tree.
    EndSession {
        /// The session's id.
        id: String,
        /// The signal to end it with; the program then ends as it does on that signal, and the session
        /// is ended by the host once it has exited.
        #[serde(default)]
        signal: Option<Signal>,
    },
}

/// The answer to one request: exactly one of `reply` and `error` is present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// The request id this answers.
    pub id: u64,
    /// The result of a call that succeeded.
    #[serde(default)]
    pub reply: Option<Reply>,
    /// Why a call failed.
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

impl Response {
    /// A success.
    pub const fn ok(id: u64, reply: Reply) -> Self {
        Self {
            id,
            reply: Some(reply),
            error: None,
        }
    }

    /// A failure.
    pub const fn failed(id: u64, error: ErrorBody) -> Self {
        Self {
            id,
            reply: None,
            error: Some(error),
        }
    }

    /// The reply, or the error the other side sent.
    ///
    /// # Errors
    ///
    /// The sent [`ErrorBody`]; a response with neither field is an `internal` error.
    pub fn into_result(self) -> Result<Reply, ErrorBody> {
        match (self.reply, self.error) {
            (Some(reply), None) => Ok(reply),
            (_, Some(error)) => Err(error),
            (None, None) => Err(ErrorBody::new(
                ErrorCode::Internal,
                "the response holds neither a reply nor an error",
            )),
        }
    }
}

/// What a successful call returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Reply {
    /// The host's side of a hello.
    Hello {
        /// The protocol version the host will speak: the lower of the two.
        version: u32,
        /// What the host does to move bytes, for a peer that cannot ask `info`.
        #[serde(default)]
        transfer: TransferInfo,
    },
    /// The host's capabilities.
    Info(Box<Info>),
    /// Metadata of one path.
    Stat(Box<Metadata>),
    /// One page of a directory.
    List(Listing),
    /// A call that returns nothing succeeded.
    Done,
    /// A symbolic link's text.
    Link {
        /// What the link holds.
        target: String,
    },
    /// The host's [`Stats`].
    Stats(Box<Stats>),
    /// The answers of a `which`: each asked name with the absolute path found, or null.
    Which {
        /// One entry for each name asked.
        found: BTreeMap<String, Option<String>>,
    },
    /// The sessions of a `sessions` call.
    Sessions {
        /// One entry per session, oldest first.
        sessions: Vec<Listed>,
    },
    /// A value this version does not know: a newer peer sent it.
    #[serde(other)]
    Other,
}

/// What a host is running on: the answer of a `stats` call. Every field is what the operating system reports now; a
/// field it cannot report is empty or `null`, never a guess.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// The machine's name, when the system has one.
    pub hostname: Option<String>,
    /// Seconds since the system started.
    pub uptime_s: u64,
    /// Logical processors this host may use.
    pub cpus: u64,
    /// The load averages over 1, 5 and 15 minutes; null where the system has none (Windows).
    pub load: Option<[f64; 3]>,
    /// Physical memory.
    pub memory: Memory,
    /// Swap or page file.
    pub swap: Swap,
    /// The mounted disks, as the system lists them.
    pub disks: Vec<Disk>,
}

/// Physical memory, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    /// All of it.
    pub total: u64,
    /// What a program can still take without swapping, caches that can be given back included.
    pub available: u64,
}

/// Swap space, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Swap {
    /// All of it.
    pub total: u64,
    /// What is unused.
    pub free: u64,
}

/// One mounted disk, in bytes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Disk {
    /// Where it is mounted: a path, or a drive such as `C:\`.
    pub mount: String,
    /// The kind of filesystem (`ext4`, `NTFS`), empty when the system does not say.
    pub filesystem: String,
    /// Its size.
    pub total: u64,
    /// What an unprivileged program can still write.
    pub available: u64,
}
