//! The crate's error and the structured error a peer sends.

use serde::{Deserialize, Serialize};

/// Why a message could not be encoded or decoded. It never quotes the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The value could not be written as CBOR.
    #[error("the message could not be encoded")]
    Encode,
    /// The bytes are not a message of the expected type.
    #[error("the bytes are not a message of the expected type")]
    Decode,
    /// A frame carries a tag this side does not know.
    #[error("the frame's tag {0} is not known")]
    Tag(u8),
    /// A frame has no tag byte.
    #[error("the frame is too short")]
    Short,
}

/// The closed list of reasons an operation fails, as the peer reports them.
///
/// A code a newer peer adds decodes as [`ErrorCode::Other`], so an old client still reads the
/// message beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// The request is malformed or names something impossible.
    BadRequest,
    /// The peer is not allowed this operation, or this program, or this path.
    Denied,
    /// The program, file or directory does not exist.
    NotFound,
    /// The target already exists.
    Exists,
    /// This host or build does not offer the operation.
    Unsupported,
    /// A configured bound is reached: the operation may succeed later.
    Busy,
    /// A path leaves the peer's filesystem roots.
    OutsideRoot,
    /// The operating system refused for another reason.
    Io,
    /// The host failed in a way the request did not cause.
    Internal,
    /// A code this version does not know.
    #[serde(other)]
    Other,
}

/// The error body of a response or a stream's error frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// What kind of failure.
    pub code: ErrorCode,
    /// A sentence for a person; it never carries a secret.
    #[serde(default)]
    pub message: String,
}

impl ErrorBody {
    /// A body with `code` and `message`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl core::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl core::error::Error for ErrorBody {}
