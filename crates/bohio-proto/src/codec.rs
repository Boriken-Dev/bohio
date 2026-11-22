//! CBOR encoding of control messages: maps with string keys, so a client in any language reads them.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Error;

/// The deepest nesting a message may have when decoded; a deeper one is [`Error::Decode`].
///
/// Every message the protocol defines nests a handful of levels, so this only bounds what a peer
/// can make the decoder recurse through.
pub const DEFAULT_DEPTH: usize = 32;

/// Writes `value` as CBOR.
///
/// # Errors
///
/// [`Error::Encode`] when the value cannot be written.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).map_err(|_| Error::Encode)?;
    Ok(bytes)
}

/// Reads a `T` from CBOR, nesting up to [`DEFAULT_DEPTH`]. Keys `T` does not name are ignored.
///
/// # Errors
///
/// [`Error::Decode`] for bytes that are not a `T`, trailing bytes included.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    decode_with_depth(bytes, DEFAULT_DEPTH)
}

/// Reads a `T` from CBOR, nesting up to `depth`.
///
/// # Errors
///
/// [`Error::Decode`] for bytes that are not a `T`, trailing bytes included, or nesting past `depth`.
pub fn decode_with_depth<T: DeserializeOwned>(bytes: &[u8], depth: usize) -> Result<T, Error> {
    let mut reader = bytes;
    let value = ciborium::de::from_reader_with_recursion_limit(&mut reader, depth)
        .map_err(|_| Error::Decode)?;
    if reader.is_empty() {
        Ok(value)
    } else {
        Err(Error::Decode)
    }
}

/// CBOR byte strings for a `Vec<u8>`.
pub(crate) mod bytes {
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

        // Not `deserialize_bytes`: ciborium reads only a byte string that fits its 4 KiB scratch buffer that way.
        deserializer.deserialize_byte_buf(Bytes)
    }
}
