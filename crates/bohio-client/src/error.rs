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
    /// The caller asked a session of one kind for another: [`Exec`](crate::exec::Exec) attached to a shell session or
    /// the reverse. The session is not touched.
    #[error("the session is of another kind than the one asked for")]
    WrongKind {
        /// The kind the host reported.
        found: bohio_proto::session::Kind,
    },
    /// The caller collected an id twice, or one that was abandoned or never sent.
    #[error("no call with that id is outstanding")]
    NothingOutstanding,
    /// The stream ended before the operation's last frame.
    #[error("the stream ended before the operation finished")]
    Closed,
    /// A compression this build cannot use: an algorithm it lacks (`zstd` without the feature, `auto` where
    /// the client must name one) or a compressed stream that could not be set up.
    #[error("compression cannot be used: {0}")]
    Compression(String),
    /// A file of this machine could not be read or written; the text says what was being done and the kind of failure,
    /// never a path.
    #[error("a local file could not be used: {0}")]
    Local(String),
    /// An update of a screen the host sent cannot be used: a rectangle outside the frame, bytes that do not decode, a size
    /// past what this side holds. The text says which; it never carries pixels.
    #[error("a screen update cannot be used: {0}")]
    Screen(String),
    /// A file rebuilt from a delta does not have the digest the host reported for it: nothing was put in place.
    #[error("the rebuilt file does not match the digest the host reported")]
    Mismatch,
}

impl Error {
    /// A local file failure: what was being done and why it failed.
    pub fn local(doing: &str, error: &std::io::Error) -> Self {
        Self::Local(format!("{doing}: {:?}", error.kind()))
    }
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
