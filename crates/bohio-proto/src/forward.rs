//! Forwarding: the frames of `bohio-connect/1`, `bohio-listen/1` and `bohio-accept/1`.
//!
//! `bohio-connect/1`: the open's metadata is a [`ConnectRequest`]. The host decides it from the peer's
//! grant before it connects anything, connects to the target, and answers with one frame: `0x01`
//! [`Answer::Connected`] or `0x05` [`Answer::Error`] (the last frame; the stream then ends). After
//! `connected` the stream carries the target's bytes both ways, raw and untagged, one message per
//! chunk, exactly as `tcp/1` of `cabuya` does: a `finish` is a half-close, and any other end of the
//! stream is a reset. The client sends nothing before `connected`.
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | host to client | `0x01` | [`Answer::Connected`] | CBOR [`Connected`] |
//! | host to client | `0x05` | [`Answer::Error`] | CBOR [`ErrorBody`] |
//! | either way, after `0x01` | none | bytes | the connection's bytes, raw |
//!
//! `bohio-listen/1` and `bohio-accept/1` are a remote forward: see [`ListenRequest`] and [`AcceptRequest`]. `bohio-accept/1`
//! answers with [`Answer`] and then carries bytes exactly as above.

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
        /// The host, as `cabuya::forward::Target` spells it: a name in lowercase or an address.
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

/// What a remote forward binds on the host: a TCP address and port (port 0 lets the host choose), a Unix socket path or a
/// Windows named pipe. The spelling is [`Target`]'s, and so is the wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Bind {
    /// A TCP listener on `host`, an address (an IPv6 one without brackets), and `port`; port 0 asks for a free one.
    Tcp {
        /// The address to bind.
        host: String,
        /// The port, or 0.
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

impl Bind {
    /// The kind's name as `info` lists it: `tcp`, `unix` or `pipe`.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Tcp { .. } => "tcp",
            Self::Unix { .. } => "unix",
            Self::Pipe { .. } => "pipe",
        }
    }
}

impl FromStr for Bind {
    type Err = BadTarget;

    /// `HOST:PORT` (an IPv6 address in brackets, the port 0 to 65535), `unix:PATH` or `pipe:NAME`.
    fn from_str(text: &str) -> Result<Self, BadTarget> {
        if text.starts_with("unix:") || text.starts_with("pipe:") {
            return Ok(match text.parse::<Target>()? {
                Target::Unix { path } => Self::Unix { path },
                Target::Pipe { name } => Self::Pipe { name },
                _ => return Err(BadTarget),
            });
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
        Ok(Self::Tcp {
            host: host.to_owned(),
            port: port.parse().map_err(|_| BadTarget)?,
        })
    }
}

impl fmt::Display for Bind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp { host, port } if host.contains(':') => write!(f, "[{host}]:{port}"),
            Self::Tcp { host, port } => write!(f, "{host}:{port}"),
            Self::Unix { path } => write!(f, "unix:{path}"),
            Self::Pipe { name } => write!(f, "pipe:{name}"),
        }
    }
}

/// The open's metadata of `bohio-listen/1`.
///
/// The host decides it from the peer's grant before it binds anything, binds, and answers with `0x01`
/// ([`ListenFrame::Listening`]) or `0x05` ([`ListenFrame::Error`], the last frame). The listener lives as long as this stream:
/// closing it, losing the link or a revoke ends the listener and the connections it handed over. Each connection the
/// listener takes is announced with `0x02` ([`ListenFrame::Arrival`]); the peer claims it by opening `bohio-accept/1` with
/// the listener's token and the arrival's id. The client sends nothing on this stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenRequest {
    /// What the host binds.
    pub bind: Bind,
    /// The most arrivals the host holds waiting for their accept, when the peer wants fewer than `[forward] max_pending`;
    /// a connection past it is closed at once. Absent: the host's bound.
    #[serde(default)]
    pub backlog: Option<u32>,
}

impl ListenRequest {
    /// A request to bind `bind`.
    pub const fn new(bind: Bind) -> Self {
        Self {
            bind,
            backlog: None,
        }
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

/// The host's `0x01` frame on a listen stream: the listener is bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listening {
    /// What was bound, in [`Bind`]'s spelling: a TCP address with the port the host chose, `unix:PATH` or `pipe:NAME`.
    pub address: String,
    /// A random 128-bit value in hex that names this listener in `bohio-accept/1`. It is not a secret between peers (the
    /// host also checks the claimant owns the listener), but it is not guessable either.
    pub token: String,
}

/// The host's `0x02` frame on a listen stream: a connection arrived and waits for its accept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arrival {
    /// The arrival's number within this listener; claimed once.
    pub id: u64,
    /// Where it came from: the connecting address of a TCP arrival; absent for a socket or a pipe.
    #[serde(default)]
    pub source: Option<String>,
}

/// A frame the host sends on a `bohio-listen/1` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ListenFrame {
    /// The listener is bound; the first frame.
    Listening(Listening),
    /// A connection is waiting.
    Arrival(Arrival),
    /// Nothing was bound, or the listener failed; the last frame.
    Error(ErrorBody),
}

const LISTENING: u8 = 0x01;
const ARRIVAL: u8 = 0x02;

impl ListenFrame {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let (tag, body) = match self {
            Self::Listening(body) => (LISTENING, encode(body)?),
            Self::Arrival(body) => (ARRIVAL, encode(body)?),
            Self::Error(body) => (ERROR, encode(body)?),
        };
        let mut frame = Vec::with_capacity(body.len() + 1);
        frame.push(tag);
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    /// Reads a frame of a listen stream.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know,
    /// [`Error::Decode`] for a CBOR body that is not what its tag says.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = frame.split_first().ok_or(Error::Short)?;
        match *tag {
            LISTENING => decode(body).map(Self::Listening),
            ARRIVAL => decode(body).map(Self::Arrival),
            ERROR => decode(body).map(Self::Error),
            other => Err(Error::Tag(other)),
        }
    }
}

/// The open's metadata of `bohio-accept/1`: claims one arrival of the peer's own listener.
///
/// The host's first frame is an [`Answer`]: `0x01` `connected` and then the arrival's bytes raw both ways exactly as
/// `bohio-connect/1` carries them, or `0x05` `not_found` for a token or id that is unknown, already claimed, expired or
/// another peer's (the answer never says which).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptRequest {
    /// The listener's token, as [`Listening::token`].
    pub token: String,
    /// The arrival's id, as [`Arrival::id`].
    pub id: u64,
}

impl AcceptRequest {
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
    /// [`Error::Decode`] when the bytes are not a request.
    pub fn from_metadata(metadata: &[u8]) -> Result<Self, Error> {
        decode(metadata)
    }
}

/// What a host reports of forwarding in `info`: whether it serves it and which kinds of target it connects
/// to and of listener it binds.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ForwardInfo {
    /// Whether the host serves forwards (`[forward] enabled`).
    pub enabled: bool,
    /// The kinds of target `bohio-connect/1` reaches here: `tcp`, and `unix` on a Unix host or `pipe` on a
    /// Windows one.
    pub connect: Vec<String>,
    /// The kinds of listener `bohio-listen/1` binds here: `tcp`, and `unix` on a Unix host or `pipe` on a Windows one.
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

    #[test]
    fn a_bind_is_a_target_that_may_name_port_zero() {
        for text in [
            "127.0.0.1:0",
            "0.0.0.0:8080",
            "[::1]:0",
            "unix:/run/user/1000/app.sock",
            "pipe:app",
        ] {
            let bind: Bind = text.parse().expect(text);
            assert_eq!(bind.to_string(), text);
        }
        for text in [
            "",
            "127.0.0.1",
            "127.0.0.1:65536",
            ":80",
            "::1:80",
            "unix:",
            "pipe:",
        ] {
            assert_eq!(text.parse::<Bind>(), Err(BadTarget), "{text}");
        }
    }

    #[test]
    fn listen_frames_round_trip_and_an_unknown_tag_is_refused() {
        let frames = [
            ListenFrame::Listening(Listening {
                address: "127.0.0.1:40000".into(),
                token: "00".repeat(16),
            }),
            ListenFrame::Arrival(Arrival {
                id: 7,
                source: Some("192.0.2.9:50000".into()),
            }),
            ListenFrame::Error(ErrorBody::new(crate::ErrorCode::InUse, "in use")),
        ];
        for frame in frames {
            assert_eq!(ListenFrame::decode(&frame.encode().unwrap()), Ok(frame));
        }
        assert_eq!(ListenFrame::decode(&[0x7f]), Err(Error::Tag(0x7f)));
        assert_eq!(ListenFrame::decode(&[]), Err(Error::Short));
    }
}
