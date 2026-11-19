//! Control calls on a `bohio/1` stream: request and response with ids, so calls overlap.

use serde::{Deserialize, Serialize};

use crate::error::{ErrorBody, ErrorCode};
use crate::fs::{Listing, Metadata, Timestamp};
use crate::info::Info;

/// A call from the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
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
        /// The client's protocol version.
        #[serde(default)]
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
    /// Copies a regular file.
    Copy {
        /// The file.
        from: String,
        /// The copy.
        to: String,
        /// Replace an existing target; otherwise `exists`.
        #[serde(default)]
        overwrite: bool,
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
    /// The SHA-256 of a file or a range of it.
    Checksum {
        /// The file.
        path: String,
        /// The first byte hashed.
        #[serde(default)]
        offset: u64,
        /// How many bytes at most; to the end when absent.
        #[serde(default)]
        length: Option<u64>,
    },
}

/// The answer to one request: exactly one of `reply` and `error` is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Reply {
    /// The host's side of a hello.
    Hello {
        /// The protocol version the host will speak: the lower of the two.
        version: u32,
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
    /// A file's digest.
    Checksum {
        /// SHA-256 as lowercase hex.
        sha256: String,
        /// How many bytes were hashed.
        length: u64,
    },
}
