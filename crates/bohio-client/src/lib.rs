#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod certificate;
pub mod connect;
pub mod control;
pub mod dial;
mod error;
pub mod exec;
pub mod fs;
pub mod link;
pub mod relay;
pub mod shell;

pub use error::Error;

use coqui_transport::Channel;

/// Options of one open that a link can honour and a plain implementation may ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct OpenOptions {
    /// The receive window this side wants on the new stream, in bytes: the most the peer may have in flight before this side
    /// reads. `None` keeps the link's own `window`. A link grants the difference on the stream, up to its `max_window`; one
    /// stream moves at most its window per round trip, so a bulk read on a long path asks for more.
    pub receive_window: Option<u32>,
}

impl OpenOptions {
    /// Options asking for a receive window of `bytes`.
    #[must_use]
    pub const fn receive_window(bytes: u32) -> Self {
        Self {
            receive_window: Some(bytes),
        }
    }
}

/// Opens one stream per operation on a link to a Bohío host.
///
/// An implementation owns the link: it opens a stream named `protocol` (see
/// `bohio_proto::protocol`) carrying `metadata` as the open's request, and returns the stream as a
/// channel. The in-process tests of the host implement it over a pair of memory channels; a
/// network client implements it over its authenticated link.
pub trait Open: Send + Sync {
    /// Opens a stream for `protocol` whose open carries `metadata`.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] or [`Error::Remote`] when the link cannot open the stream.
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn Channel>, Error>;

    /// As [`Open::open`], with `options`. The default ignores them, which is right for a link whose streams
    /// have no window.
    ///
    /// # Errors
    ///
    /// As [`Open::open`].
    fn open_with(
        &self,
        protocol: &str,
        metadata: &[u8],
        options: &OpenOptions,
    ) -> Result<Box<dyn Channel>, Error> {
        let _ = options;
        self.open(protocol, metadata)
    }

    /// The receive window this link asks for on a stream that carries a file's bytes to this side (`[link] bulk_window`),
    /// when it asks for more than its own window. Reads use it. The default is `None`.
    fn bulk_window(&self) -> Option<u32> {
        None
    }
}
