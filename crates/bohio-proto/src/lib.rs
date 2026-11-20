#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod codec;
pub mod control;
pub mod error;
pub mod exec;
pub mod fs;
pub mod hash;
pub mod info;
pub mod local;
pub mod shell;

pub use codec::{DEFAULT_DEPTH, decode, decode_with_depth, encode};
pub use error::{Error, ErrorBody, ErrorCode};
pub use info::PROTOCOL_VERSION;

/// The protocol names a stream is opened with; the open's metadata is the request.
pub mod protocol {
    /// Control calls: hello, info and the small filesystem calls.
    pub const CONTROL: &str = "bohio/1";
    /// One process, run to its exit.
    pub const EXEC: &str = "bohio-exec/1";
    /// One interactive shell on a terminal.
    pub const SHELL: &str = "bohio-shell/1";
    /// One file read, streamed.
    pub const READ: &str = "bohio-read/1";
    /// One file written, streamed.
    pub const WRITE: &str = "bohio-write/1";
    /// Digests of many files, streamed.
    pub const HASH: &str = "bohio-hash/1";
}
