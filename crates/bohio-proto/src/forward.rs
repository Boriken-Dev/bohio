//! Forwarding: the frames of `bohio-connect/1`.
//!
//! `bohio-connect/1`: the open's metadata is a [`ConnectRequest`]. The host decides it from the peer's
//! grant before it connects anything, connects to the target, and answers with one frame: `0x01`
//! [`Answer::Connected`] or `0x05` [`Answer::Error`] (the last frame; the stream then ends). After
//! `connected` the stream carries the target's bytes both ways, raw and untagged, one message per
//! chunk, exactly as `tcp/1` of `coqui-link` does: a `finish` is a half-close, and any other end of the
//! stream is a reset. The client sends nothing before `connected`.
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | host to client | `0x01` | [`Answer::Connected`] | CBOR [`Connected`] |
//! | host to client | `0x05` | [`Answer::Error`] | CBOR [`ErrorBody`] |
//! | either way, after `0x01` | none | bytes | the connection's bytes, raw |

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorBody};
use crate::{decode, encode};

/// What a forward reaches. `serial` is reserved as a kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Target {
    /// A TCP connection to `host` (a name, or an address; an IPv6 address without brackets) and `port`.
    Tcp {
        /// The host, as `coqui_link::forward::Target` spells it: a name in lowercase or an address.
        host: String,
        /// The port; not zero.
        port: u16,
    },
    /// A Unix domain socket at `path` on the host.
    Unix {
        /// The socket's path in the host's own text.
        path: String,
    },
    /// A Windows named pipe, `NAME` of `\\.\pipe\NAME`, on the host.
    Pipe {
        /// The pipe's name.
        name: String,
    },
}

/// Text that does not name a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadTarget;

impl fmt::Display for BadTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "a target is HOST:PORT, unix:PATH or pipe:NAME, with an IPv6 address in brackets",
        )
    }
}

impl core::error::Error for BadTarget {}

impl Target {
    /// The kind's name as `info` lists it: `tcp`, `unix` or `pipe`.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Tcp { .. } => "tcp",
            Self::Unix { .. } => "unix",
            Self::Pipe { .. } => "pipe",
        }
    }
}

impl FromStr for Target {
    type Err = BadTarget;

    /// `HOST:PORT` (an IPv6 address in brackets), `unix:PATH` or `pipe:NAME`. A `PATH` or a `NAME` is taken as
    /// written; only emptiness is refused here, the host decides the rest.
    fn from_str(text: &str) -> Result<Self, BadTarget> {
        if let Some(path) = text.strip_prefix("unix:") {
            return if path.is_empty() {
                Err(BadTarget)
            } else {
                Ok(Self::Unix {
                    path: path.to_owned(),
                })
            };
        }
        if let Some(name) = text.strip_prefix("pipe:") {
            return if name.is_empty() {
                Err(BadTarget)
            } else {
                Ok(Self::Pipe {
                    name: name.to_owned(),
                })
            };
        }
        let (host, port) = text.rsplit_once(':').ok_or(BadTarget)?;
        let host = match host.strip_prefix('[') {
            Some(inner) => {
                let inner = inner.strip_suffix(']').ok_or(BadTarget)?;
                if !inner.contains(':') {
                    return Err(BadTarget);
                }
                inner
            }
            None if host.contains(':') => return Err(BadTarget),
            None => host,
        };
        if host.is_empty() || port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(BadTarget);
        }
        match port.parse::<u16>() {
            Ok(port) if port != 0 => Ok(Self::Tcp {
                host: host.to_owned(),
                port,
            }),
            _ => Err(BadTarget),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp { host, port } if host.contains(':') => write!(f, "[{host}]:{port}"),
            Self::Tcp { host, port } => write!(f, "{host}:{port}"),
            Self::Unix { path } => write!(f, "unix:{path}"),
            Self::Pipe { name } => write!(f, "pipe:{name}"),
        }
    }
}

/// The open's metadata of `bohio-connect/1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectRequest {
    /// Where the host connects.
    pub target: Target,
}

impl ConnectRequest {
    /// A request for `target`.
    pub const fn new(target: Target) -> Self {
        Self { target }
    }

    /// The open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the request cannot be written.
    pub fn to_metadata(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a request from an open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not a request, or name a kind this version does not know.
    pub fn from_metadata(metadata: &[u8]) -> Result<Self, Error> {
        decode(metadata)
    }
}

/// The host's `0x01` frame: the connection is made and the stream now carries its bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connected {
    /// The address the host's connection reached, when it has one (a TCP target's resolved address);
    /// absent for a socket or a pipe.
    #[serde(default)]
    pub address: Option<String>,
}

/// The host's first frame.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Answer {
    /// Connected: raw bytes follow in both directions.
    Connected(Connected),
    /// Nothing was connected; the last frame.
    Error(ErrorBody),
}

const CONNECTED: u8 = 0x01;
const ERROR: u8 = 0x05;

impl Answer {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let (tag, body) = match self {
            Self::Connected(body) => (CONNECTED, encode(body)?),
            Self::Error(body) => (ERROR, encode(body)?),
        };
        let mut frame = Vec::with_capacity(body.len() + 1);
        frame.push(tag);
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    /// Reads the first frame of a connect stream.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know,
    /// [`Error::Decode`] for a CBOR body that is not what its tag says.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = frame.split_first().ok_or(Error::Short)?;
        match *tag {
            CONNECTED => decode(body).map(Self::Connected),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}

/// What a host reports of forwarding in `info`: whether it serves it and which kinds of target it connects
/// to. `listen` is empty until the host serves remote forwards.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ForwardInfo {
    /// Whether the host serves forwards (`[forward] enabled`).
    pub enabled: bool,
    /// The kinds of target `bohio-connect/1` reaches here: `tcp`, and `unix` on a Unix host or `pipe` on a
    /// Windows one.
    pub connect: Vec<String>,
    /// The kinds a remote forward binds.
    pub listen: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_read_and_written_in_one_spelling() {
        for text in [
            "db.lan:5432",
            "192.0.2.5:22",
            "[fd00::5]:443",
            "unix:/run/docker.sock",
            "pipe:docker_engine",
        ] {
            let target: Target = text.parse().expect(text);
            assert_eq!(target.to_string(), text);
        }
    }

    #[test]
    fn text_that_is_not_a_target_is_refused() {
        for text in [
            "",
            "db.lan",
            "db.lan:0",
            "db.lan:65536",
            "db.lan:http",
            ":80",
            "fd00::5:80",
            "[fd00::5:80",
            "[db.lan]:80",
            "unix:",
            "pipe:",
        ] {
            assert_eq!(text.parse::<Target>(), Err(BadTarget), "{text}");
        }
    }
}
