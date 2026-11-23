#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod certificate;
pub mod connect;
pub mod control;
pub mod dial;
mod error;
pub mod exec;
pub mod forward;
pub mod fs;
pub mod gui;
pub mod hash;
pub mod keep;
pub mod link;
pub mod relay;
pub mod session;
pub mod shell;
pub mod sync;

pub use error::Error;

use coqui_link::mux::LinkStream;

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
/// [`LinkStream`]: a channel that can also finish (half-close) and say how much of it is left. The in-process tests of
/// the host implement it over a pair of memory channels wrapped in [`relay::Plain`]; a network client implements it
/// over its authenticated link.
pub trait Open: Send + Sync {
    /// Opens a stream for `protocol` whose open carries `metadata`.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] or [`Error::Remote`] when the link cannot open the stream.
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error>;

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
    ) -> Result<Box<dyn LinkStream>, Error> {
        let _ = options;
        self.open(protocol, metadata)
    }

    /// The receive window this link asks for on a stream that carries a file's bytes to this side (`[link] bulk_window`),
    /// when it asks for more than its own window. Reads use it. The default is `None`.
    fn bulk_window(&self) -> Option<u32> {
        None
    }

    /// Whether the link can still open streams. A listener that serves a forward ends when it is not. The default is `true`:
    /// a link that cannot tell finds out when an open fails.
    fn is_up(&self) -> bool {
        true
    }

    /// As [`Open::open`], with a [`connect::Cause`] that says, once the stream ends, why: whether the host refused it because
    /// it was over a limit (which a caller may retry) or for another reason. The default cannot tell and answers an
    /// [`connect::Cause::unknown`].
    ///
    /// # Errors
    ///
    /// As [`Open::open`].
    fn open_cause(
        &self,
        protocol: &str,
        metadata: &[u8],
    ) -> Result<(Box<dyn LinkStream>, connect::Cause), Error> {
        self.open(protocol, metadata)
            .map(|stream| (stream, connect::Cause::unknown()))
    }

    /// How long an open waits for a link to come back when there is none, for a link that is kept ([`keep::Relink`]); `None`
    /// for a link that is what it is. A forward retries a connect whose link was lost before the host answered only when
    /// this is `Some`.
    fn relink_wait(&self) -> Option<std::time::Duration> {
        None
    }
}
