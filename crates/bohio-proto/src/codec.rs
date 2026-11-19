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
