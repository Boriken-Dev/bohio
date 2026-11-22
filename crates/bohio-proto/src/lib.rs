#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod codec;
pub mod compress;
pub mod control;
pub mod error;
pub mod exec;
pub mod forward;
pub mod fs;
pub mod gui;
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
    /// One connection from the host to a target, then its raw bytes both ways.
    pub const CONNECT: &str = "bohio-connect/1";
    /// `coqui-link`'s `tcp/1`, served under the same grant as [`CONNECT`]: the open's metadata is `HOST:PORT`
    /// text and the stream is raw bytes at once, with no answer before them.
    pub const TCP: &str = "tcp/1";
    /// One view of a desktop: snapshots, keyboard and mouse input and the clipboard's text.
    pub const GUI: &str = "bohio-gui/1";
    /// One listener on the host for a peer; each arrival is announced on the stream.
    pub const LISTEN: &str = "bohio-listen/1";
    /// One arrival of a [`LISTEN`] listener, then its raw bytes both ways.
    pub const ACCEPT: &str = "bohio-accept/1";
}
