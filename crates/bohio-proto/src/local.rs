//! The local control endpoint of a running node: two protocols spoken only on its local socket, never on a
//! peer's link.
//!
//! [`VIA`] carries one stream of another protocol to a peer the node has a link to, whichever side made the link:
//! the local client opens `bohio-via/1` with a [`ViaRequest`] and then speaks the named protocol through it
//! unchanged. [`LINKS`] lists the node's links in one [`LinksReply`]. [`PAIR`] arms the node's pairing: the node
//! sends a PIN, and then who paired with it or why nobody did.

use serde::{Deserialize, Serialize};

use crate::codec::{decode, encode};
use crate::error::Error;

/// Opens a stream of another protocol on a link the node has to a peer.
pub const VIA: &str = "bohio-via/1";

/// Lists the node's links.
pub const LINKS: &str = "bohio-links/1";

/// Lists the provider libraries the node loaded at start (`[providers] paths`).
pub const PROVIDERS: &str = "bohio-providers/1";

/// One provider library, as the node's start found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// The library file, or the configured path that named nothing.
    pub path: String,
    /// `loaded`, `skipped` or `failed`.
    pub state: String,
    /// Why, or what the library offers.
    pub detail: String,
    /// `sha256:` and the digest of the file's bytes, when it was read.
    #[serde(default)]
    pub digest: Option<String>,
    /// What `[providers] check_access = false` waived for this library.
    #[serde(default)]
    pub unchecked: Vec<String>,
}

/// The reply of a [`PROVIDERS`] stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvidersReply {
    /// `[providers] check_access`: `false` is the non-default choice.
    pub check_access: bool,
    /// Each library, in the order the paths were written.
    #[serde(default)]
    pub libraries: Vec<LibraryEntry>,
}

impl ProvidersReply {
    /// Writes the reply as one message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the reply cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a reply from one message.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not a reply.
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        decode(message)
    }
}

/// Arms the node's one PIN for a peer to pair on any of the node's listeners.
pub const PAIR: &str = "bohio-pair/1";

/// Lists the desktop approvals a node waits for and takes the owner's decisions (`bohio-gui-approve/1`).
pub const GUI_APPROVE: &str = "bohio-gui-approve/1";

/// What a per-user GUI helper opens on its node, over the helper endpoint (never on a peer's link): its session, then its
/// state and the audit events of the streams it serves. The node opens `bohio-gui/1` streams on the same connection.
pub const GUI_HELPER: &str = "bohio-gui-helper/1";

/// The metadata of a [`VIA`] open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViaRequest {
    /// The peer's id, as `bohio id` prints it.
    pub peer: String,
    /// The protocol to open on the link to that peer, for example `bohio-exec/1`.
    pub protocol: String,
    /// That protocol's open metadata, passed unchanged.
    #[serde(with = "crate::codec::bytes", default)]
    pub metadata: Vec<u8>,
}

impl ViaRequest {
    /// Writes the request as an open's metadata.
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

/// The metadata of a [`LINKS`] open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LinksRequest {
    /// How long, in milliseconds, to wait for each peer's `link/1` document (`theirs`); the node's own
    /// `[link] proof_timeout` when absent. A peer that does not answer in time has no `theirs`.
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

impl LinksRequest {
    /// Writes the request as an open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the request cannot be written.
    pub fn to_metadata(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a request from an open's metadata; empty metadata is the defaults.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not a request.
    pub fn from_metadata(metadata: &[u8]) -> Result<Self, Error> {
        if metadata.is_empty() {
            Ok(Self::default())
        } else {
            decode(metadata)
        }
    }
}

/// The metadata of a [`PAIR`] open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PairRequest {
    /// How long, in milliseconds, the PIN lasts; the node's `[link] pair_timeout` when absent.
    #[serde(default)]
    pub lifetime_ms: Option<u64>,
}

impl PairRequest {
    /// Writes the request as an open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the request cannot be written.
    pub fn to_metadata(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a request from an open's metadata; empty metadata is the defaults.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not a request.
    pub fn from_metadata(metadata: &[u8]) -> Result<Self, Error> {
        if metadata.is_empty() {
            Ok(Self::default())
        } else {
            decode(metadata)
        }
    }
}

/// What a [`PAIR`] stream says: first [`PairReply::Armed`], then one of the others, then the end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairReply {
    /// The PIN is armed: it is for the person who reads it out, and is in no log or event.
    Armed {
        /// The PIN.
        pin: String,
        /// How long it lasts, in milliseconds.
        lifetime_ms: u64,
    },
    /// A peer paired with it; its `[peer.<id>]` table was written and holds nothing.
    Paired {
        /// The peer's id.
        peer: String,
    },
    /// Nobody paired: the PIN is used up, expired, replaced by another request or withdrawn.
    Ended {
        /// Why, with no key or PIN in it.
        reason: String,
    },
}

impl PairReply {
    /// Writes the reply as one message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the reply cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a reply from one message.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not a reply.
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        decode(message)
    }
}

/// One link of the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkEntry {
    /// The peer's id.
    pub peer: String,
    /// `dialed` when this node made the link, `answered` when the peer did.
    pub side: String,
    /// The endpoint this node dialed, for a dialed link.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// The peer's network address, when the node knows it.
    #[serde(default)]
    pub source: Option<String>,
    /// When the link came up, in seconds since the Unix epoch.
    pub since: u64,
    /// The protocols this node offers the peer now.
    #[serde(default)]
    pub mine: Vec<String>,
    /// The protocols the peer offers this node, read from its `link/1` document just now; absent when it did not
    /// answer in time.
    #[serde(default)]
    pub theirs: Option<Vec<String>>,
}

/// The reply of a [`LINKS`] stream: every link that is up, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LinksReply {
    /// The links.
    #[serde(default)]
    pub links: Vec<LinkEntry>,
}

impl LinksReply {
    /// Writes the reply as one message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the reply cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a reply from one message.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not a reply.
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        decode(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_via_request_carries_its_metadata_as_a_cbor_byte_string() {
        let request = ViaRequest {
            peer: "sha256:00".to_owned(),
            protocol: "bohio-exec/1".to_owned(),
            metadata: vec![1, 2, 3, 255],
        };
        let wire = request.to_metadata().unwrap();
        let value: ciborium::Value = ciborium::from_reader(&wire[..]).unwrap();
        let ciborium::Value::Map(entries) = value else {
            panic!("a map");
        };
        let metadata = entries
            .iter()
            .find(|(key, _)| key.as_text() == Some("metadata"))
            .map(|(_, value)| value.clone())
            .unwrap();
        assert_eq!(metadata, ciborium::Value::Bytes(vec![1, 2, 3, 255]));
        assert_eq!(ViaRequest::from_metadata(&wire).unwrap(), request);
    }

    #[test]
    fn a_links_request_may_be_empty_and_a_reply_round_trips() {
        assert_eq!(
            LinksRequest::from_metadata(&[]).unwrap(),
            LinksRequest::default()
        );
        let request = LinksRequest { wait_ms: Some(250) };
        assert_eq!(
            LinksRequest::from_metadata(&request.to_metadata().unwrap()).unwrap(),
            request
        );
        let reply = LinksReply {
            links: vec![LinkEntry {
                peer: "sha256:00".to_owned(),
                side: "dialed".to_owned(),
                endpoint: Some("tcp://host.example:7620".to_owned()),
                source: None,
                since: 1_700_000_000,
                mine: vec!["bohio/1".to_owned()],
                theirs: None,
            }],
        };
        assert_eq!(LinksReply::decode(&reply.encode().unwrap()).unwrap(), reply);
    }
}
