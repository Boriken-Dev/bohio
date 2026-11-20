//! The local control endpoint of a running node: two protocols spoken only on its local socket, never on a
//! peer's link.
//!
//! [`VIA`] carries one stream of another protocol to a peer the node has a link to, whichever side made the link:
//! the local client opens `bohio-via/1` with a [`ViaRequest`] and then speaks the named protocol through it
//! unchanged. [`LINKS`] lists the node's links in one [`LinksReply`].

use serde::{Deserialize, Serialize};

use crate::codec::{decode, encode};
use crate::error::Error;

/// Opens a stream of another protocol on a link the node has to a peer.
pub const VIA: &str = "bohio-via/1";

/// Lists the node's links.
pub const LINKS: &str = "bohio-links/1";

/// The metadata of a [`VIA`] open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViaRequest {
    /// The peer's id, as `bohio id` prints it.
    pub peer: String,
    /// The protocol to open on the link to that peer, for example `bohio-exec/1`.
    pub protocol: String,
    /// That protocol's open metadata, passed unchanged.
    #[serde(with = "bytes", default)]
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

/// CBOR byte strings for a `Vec<u8>`.
mod bytes {
    use serde::Serializer;
    use serde::de::{self, Deserializer, SeqAccess, Visitor};

    pub fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        struct Bytes;

        impl<'de> Visitor<'de> for Bytes {
            type Value = Vec<u8>;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("a byte string")
            }

            fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Vec<u8>, E> {
                Ok(value.to_vec())
            }

            fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(value)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::new();
                while let Some(byte) = seq.next_element::<u8>()? {
                    out.push(byte);
                }
                Ok(out)
            }
        }

        deserializer.deserialize_bytes(Bytes)
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
