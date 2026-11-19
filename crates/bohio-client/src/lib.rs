#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
pub mod exec;

pub use error::Error;

use coqui_transport::Channel;

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
}
