//! Block signatures: the weak and strong hash of each block of a file, which is what a delta is matched against.

use bohio_proto::hash::Algorithm;

use crate::hash::Hasher;
use crate::rolling::Rolling;

pub use bohio_proto::fs::WEAK_RSYNC;

/// Why a signature's parameters cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SpecError {
    /// The block size is zero.
    #[error("the block size must be at least 1")]
    BlockSize,
    /// The strong hash is not an algorithm this build computes.
    #[error("this build does not compute {0}")]
    Algorithm(Algorithm),
    /// The strong hash length is zero or longer than the algorithm's digest.
    #[error("the strong hash length is from 1 to the digest's {0} bytes")]
    StrongLen(usize),
    /// The weak checksum is not `rsync`.
    #[error("the weak checksum `{0}` is not known")]
    Weak(String),
    /// A batch of strong hashes does not hold one of the signature's length per weak value.
    #[error("the strong hashes do not match the weak values")]
    Batch,
    /// More blocks than a signature can number.
    #[error("the signature holds more blocks than it can number")]
    TooMany,
    /// The signature does not hold every block its size needs.
    #[error("the signature is not complete")]
    Incomplete,
}

/// The block size for a file of `size` bytes: the square root rounded up to a power of two, between
/// `min` and `max` (`min` wins when `max` is below it).
pub fn block_size_for(size: u64, min: u32, max: u32) -> u32 {
    let wanted = size.isqrt().max(1).next_power_of_two();
    let min = u64::from(min.max(1));
    let max = u64::from(max).max(min);
    u32::try_from(wanted.clamp(min, max)).unwrap_or(u32::MAX)
}

/// How a signature is made: the block size and the strong hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureSpec {
    /// The bytes in each block but the last, which may be shorter.
    pub block_size: u32,
    /// The strong hash of each block.
    pub strong: Algorithm,
    /// How many leading bytes of each strong hash are kept.
    pub strong_len: usize,
}

impl SignatureSpec {
    /// A spec after checking it.
    ///
    /// # Errors
    ///
    /// [`SpecError`] for a zero block size, an algorithm this build lacks, or a length of zero or past the digest.
    pub fn new(block_size: u32, strong: Algorithm, strong_len: usize) -> Result<Self, SpecError> {
        let spec = Self {
            block_size,
            strong,
            strong_len,
        };
        spec.check()?;
        Ok(spec)
    }

    /// The length a strong hash of `algorithm` has, in bytes: the default `strong_len` is this, up to 16.
    pub fn digest_len(algorithm: &Algorithm) -> Option<usize> {
        algorithm.hex_len().map(|digits| digits / 2)
    }

    /// Checks the spec.
    ///
    /// # Errors
    ///
    /// As [`SignatureSpec::new`].
    pub fn check(&self) -> Result<(), SpecError> {
        if self.block_size == 0 {
            return Err(SpecError::BlockSize);
        }
        let digest = Self::digest_len(&self.strong)
            .filter(|_| Hasher::new(std::slice::from_ref(&self.strong)).is_ok())
            .ok_or_else(|| SpecError::Algorithm(self.strong.clone()))?;
        if self.strong_len == 0 || self.strong_len > digest {
            return Err(SpecError::StrongLen(digest));
        }
        Ok(())
    }

    /// The strong hash of `block`, cut to `strong_len` bytes.
    ///
    /// # Panics
    ///
    /// Never for a spec that passed [`SignatureSpec::check`].
    pub fn strong_of(&self, block: &[u8]) -> Vec<u8> {
        let mut hasher = Hasher::new(std::slice::from_ref(&self.strong))
            .expect("the spec was checked: its algorithm is known");
        hasher.update(block);
        let digest = hasher
            .finish()
            .into_values()
            .next()
            .expect("one algorithm gives one digest");
        let mut bytes = unhex(&digest);
        bytes.truncate(self.strong_len);
        bytes
    }
}

fn unhex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => 0,
            };
            digit(pair[0]) << 4 | digit(pair.get(1).copied().unwrap_or(b'0'))
        })
        .collect()
}

/// The weak and strong hash of one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSig {
    /// [`Rolling`]'s value over the block.
    pub weak: u32,
    /// The strong hash, `strong_len` bytes.
    pub strong: Vec<u8>,
}

/// Makes the signature of a file from its bytes, a piece at a time: it holds at most one block.
#[derive(Debug)]
pub struct SignatureBuilder {
    spec: SignatureSpec,
    partial: Vec<u8>,
}

impl SignatureBuilder {
    /// A builder for `spec`.
    ///
    /// # Errors
    ///
    /// [`SpecError`] when the spec does not pass [`SignatureSpec::check`].
    pub fn new(spec: SignatureSpec) -> Result<Self, SpecError> {
        spec.check()?;
        Ok(Self {
            partial: Vec::new(),
            spec,
        })
    }

    /// The spec in use.
    pub const fn spec(&self) -> &SignatureSpec {
        &self.spec
    }

    fn sign(&self, block: &[u8]) -> BlockSig {
        BlockSig {
            weak: Rolling::new(block).value(),
            strong: self.spec.strong_of(block),
        }
    }

    /// Feeds the next bytes of the file; returns the signature of each block they complete.
    pub fn update(&mut self, mut bytes: &[u8]) -> Vec<BlockSig> {
        let size = self.spec.block_size as usize;
        let mut done = Vec::new();
        if !self.partial.is_empty() {
            let take = (size - self.partial.len()).min(bytes.len());
            self.partial.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.partial.len() == size {
                let block = std::mem::take(&mut self.partial);
                done.push(self.sign(&block));
            }
        }
        while bytes.len() >= size {
            let (block, rest) = bytes.split_at(size);
            done.push(self.sign(block));
            bytes = rest;
        }
        if !bytes.is_empty() {
            self.partial.extend_from_slice(bytes);
        }
        done
    }

    /// The end of the file: the signature of the last, shorter block when there is one.
    pub fn finish(self) -> Option<BlockSig> {
        (!self.partial.is_empty()).then(|| self.sign(&self.partial))
    }
}

/// The signature of a whole file: what a [`Matcher`](crate::delta::Matcher) matches against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    spec: SignatureSpec,
    size: u64,
    weak: Vec<u32>,
    strong: Vec<u8>,
}

impl Signature {
    /// An empty signature of a file of `size` bytes, to be filled block by block.
    ///
    /// # Errors
    ///
    /// [`SpecError`] when the spec does not pass [`SignatureSpec::check`].
    pub fn new(spec: SignatureSpec, size: u64) -> Result<Self, SpecError> {
        spec.check()?;
        Ok(Self {
            spec,
            size,
            weak: Vec::new(),
            strong: Vec::new(),
        })
    }

    /// The spec.
    pub const fn spec(&self) -> &SignatureSpec {
        &self.spec
    }

    /// The size of the file described.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// How many blocks a file of this size has.
    pub fn blocks_needed(&self) -> u64 {
        self.size.div_ceil(u64::from(self.spec.block_size))
    }

    /// How many blocks were added.
    pub fn blocks(&self) -> u64 {
        self.weak.len() as u64
    }

    /// Whether every block of the file is there.
    pub fn is_complete(&self) -> bool {
        self.blocks() == self.blocks_needed()
    }

    /// How many bytes the signature takes on the wire: a weak value and a strong hash per block.
    pub fn wire_len(&self) -> u64 {
        self.blocks() * (4 + self.spec.strong_len as u64)
    }

    /// Adds the next block.
    ///
    /// # Errors
    ///
    /// [`SpecError::Batch`] when the strong hash is not `strong_len` bytes, [`SpecError::TooMany`] past the
    /// blocks the size needs.
    pub fn push(&mut self, block: &BlockSig) -> Result<(), SpecError> {
        self.push_batch(&[block.weak], &block.strong)
    }

    /// Adds the next blocks: their weak values, and their strong hashes joined.
    ///
    /// # Errors
    ///
    /// As [`Signature::push`].
    pub fn push_batch(&mut self, weak: &[u32], strong: &[u8]) -> Result<(), SpecError> {
        if strong.len() != weak.len() * self.spec.strong_len {
            return Err(SpecError::Batch);
        }
        if self.blocks() + weak.len() as u64 > self.blocks_needed() {
            return Err(SpecError::TooMany);
        }
        self.weak.extend_from_slice(weak);
        self.strong.extend_from_slice(strong);
        Ok(())
    }

    pub(crate) fn weak(&self) -> &[u32] {
        &self.weak
    }

    pub(crate) fn strong(&self, block: usize) -> &[u8] {
        let length = self.spec.strong_len;
        &self.strong[block * length..(block + 1) * length]
    }

    /// The length of the file's last block: the block size, or the remainder.
    pub fn last_len(&self) -> u32 {
        let size = u64::from(self.spec.block_size);
        match self.size % size {
            0 if self.size == 0 => 0,
            0 => self.spec.block_size,
            rest => u32::try_from(rest).unwrap_or(self.spec.block_size),
        }
    }
}
