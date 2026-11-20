//! The client's error.

use bohio_proto::ErrorBody;
use coqui_transport::Failure;

/// Why a client call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The host refused or failed the operation.
    #[error("the host reported an error: {0}")]
    Remote(ErrorBody),
    /// The channel failed.
    #[error("the channel failed: {0}")]
    Transport(Failure),
    /// A message from the host could not be read.
    #[error("a message from the host could not be read: {0}")]
    Protocol(bohio_proto::Error),
    /// Nothing arrived in time; the exchange goes on.
    #[error("nothing arrived in time")]
    Timeout,
    /// The host could not be proven or would not prove this side; the text says why, never a key.
    #[error("the link could not be established: {0}")]
    Link(String),
    /// The stream ended before the operation's last frame.
    #[error("the stream ended before the operation finished")]
    Closed,
    /// A compression this build cannot use: an algorithm it lacks (`zstd` without the feature, `auto` where
    /// the client must name one) or a compressed stream that could not be set up.
    #[error("compression cannot be used: {0}")]
    Compression(String),
}

impl From<Failure> for Error {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Timeout => Self::Timeout,
            other => Self::Transport(other),
        }
    }
}

impl From<bohio_proto::Error> for Error {
    fn from(error: bohio_proto::Error) -> Self {
        Self::Protocol(error)
    }
}
