//! Digests of several algorithms computed in one pass over the bytes.

use std::io::{self, Read};

use blake3::Hasher as Blake3;
use bohio_proto::hash::{Algorithm, Digests};
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use xxhash_rust::xxh3::Xxh3;
use xxhash_rust::xxh64::Xxh64;

/// Every algorithm this build computes, in the order [`Algorithm::KNOWN`] lists them.
pub fn available() -> Vec<Algorithm> {
    Algorithm::KNOWN.to_vec()
}

/// An algorithm a [`Hasher`] was asked for and this build does not compute.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("this build does not compute {0}")]
pub struct Unsupported(pub Algorithm);

enum State {
    Md5(Md5),
    Sha1(Sha1),
    Sha256(Sha256),
    Sha384(Sha384),
    Sha512(Sha512),
    Blake3(Box<Blake3>),
    Xxh3(Box<Xxh3>),
    Xxh128(Box<Xxh3>),
    Xxh64(Box<Xxh64>),
    Crc32c(u32),
}

impl State {
    fn new(algorithm: &Algorithm) -> Result<Self, Unsupported> {
        Ok(match algorithm {
            Algorithm::Md5 => Self::Md5(Md5::new()),
            Algorithm::Sha1 => Self::Sha1(Sha1::new()),
            Algorithm::Sha256 => Self::Sha256(Sha256::new()),
            Algorithm::Sha384 => Self::Sha384(Sha384::new()),
            Algorithm::Sha512 => Self::Sha512(Sha512::new()),
            Algorithm::Blake3 => Self::Blake3(Box::new(Blake3::new())),
            Algorithm::Xxh3 => Self::Xxh3(Box::new(Xxh3::new())),
            Algorithm::Xxh128 => Self::Xxh128(Box::new(Xxh3::new())),
            Algorithm::Xxh64 => Self::Xxh64(Box::new(Xxh64::new(0))),
            Algorithm::Crc32c => Self::Crc32c(0),
            other => return Err(Unsupported(other.clone())),
        })
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Md5(state) => Digest::update(state, bytes),
            Self::Sha1(state) => Digest::update(state, bytes),
            Self::Sha256(state) => Digest::update(state, bytes),
            Self::Sha384(state) => Digest::update(state, bytes),
            Self::Sha512(state) => Digest::update(state, bytes),
            Self::Blake3(state) => {
                state.update(bytes);
            }
            Self::Xxh3(state) | Self::Xxh128(state) => state.update(bytes),
            Self::Xxh64(state) => state.update(bytes),
            Self::Crc32c(state) => *state = crc32c::crc32c_append(*state, bytes),
        }
    }

    fn finish(self) -> String {
        match self {
            Self::Md5(state) => hex(&state.finalize()),
            Self::Sha1(state) => hex(&state.finalize()),
            Self::Sha256(state) => hex(&state.finalize()),
            Self::Sha384(state) => hex(&state.finalize()),
            Self::Sha512(state) => hex(&state.finalize()),
            Self::Blake3(state) => hex(state.finalize().as_bytes()),
            Self::Xxh3(state) => format!("{:016x}", state.digest()),
            Self::Xxh128(state) => format!("{:032x}", state.digest128()),
            Self::Xxh64(state) => format!("{:016x}", state.digest()),
            Self::Crc32c(state) => format!("{state:08x}"),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// Computes several digests in one pass: feed it the bytes once and every algorithm sees them.
pub struct Hasher {
    states: Vec<(Algorithm, State)>,
}

impl core::fmt::Debug for Hasher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list()
            .entries(self.states.iter().map(|(algorithm, _)| algorithm))
            .finish()
    }
}

impl Hasher {
    /// A hasher for `algorithms`; one named twice is computed once.
    ///
    /// # Errors
    ///
    /// [`Unsupported`] for an algorithm this build does not compute ([`Algorithm::Other`]).
    pub fn new(algorithms: &[Algorithm]) -> Result<Self, Unsupported> {
        let mut states: Vec<(Algorithm, State)> = Vec::with_capacity(algorithms.len());
        for algorithm in algorithms {
            if states.iter().all(|(have, _)| have != algorithm) {
                states.push((algorithm.clone(), State::new(algorithm)?));
            }
        }
        Ok(Self { states })
    }

    /// Whether no algorithm was asked for.
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Feeds `bytes` to every algorithm.
    pub fn update(&mut self, bytes: &[u8]) {
        for (_, state) in &mut self.states {
            state.update(bytes);
        }
    }

    /// The digest of everything fed, lowercase hex, by algorithm.
    pub fn finish(self) -> Digests {
        self.states
            .into_iter()
            .map(|(algorithm, state)| (algorithm, state.finish()))
            .collect()
    }
}

/// Reads `reader` to its end through a [`Hasher`] for `algorithms`, `buffer_size` bytes at a time (at
/// least 1); returns the digests and how many bytes were read.
///
/// # Errors
///
/// The reader's error; an unsupported algorithm is [`io::ErrorKind::Unsupported`].
pub fn digest_reader(
    reader: &mut impl Read,
    algorithms: &[Algorithm],
    buffer_size: usize,
) -> io::Result<(Digests, u64)> {
    let mut hasher = Hasher::new(algorithms)
        .map_err(|error| io::Error::new(io::ErrorKind::Unsupported, error))?;
    let mut buffer = vec![0_u8; buffer_size.max(1)];
    let mut total = 0_u64;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok((hasher.finish(), total)),
            Ok(count) => {
                hasher.update(&buffer[..count]);
                total += count as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}
