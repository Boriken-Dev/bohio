//! Rebuilding the new file: carrying out the steps of a delta against the old file.

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::delta::Op;

/// Why a step could not be carried out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ApplyError {
    /// A copy names blocks the old file does not have, or none.
    #[error("a copy reaches past the end of the old file")]
    PastEnd,
    /// Reading the old file or writing the new one failed.
    #[error("reading or writing failed: {0:?}")]
    Io(io::ErrorKind),
    /// A copy would take the output past the bound.
    #[error("the new file grows past what was allowed")]
    TooLong,
}

impl From<io::Error> for ApplyError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// The bytes a copy of `count` blocks from `block` is, in an old file of `base_len` bytes; `None` when it reaches past
/// the end (the last block of the old file may be shorter than `block_size`, and a copy may end in it).
pub fn copy_len(base_len: u64, block_size: u32, block: u64, count: u64) -> Option<u64> {
    let size = u64::from(block_size);
    let blocks = base_len.div_ceil(size);
    if count == 0 || block >= blocks || count > blocks - block {
        return None;
    }
    let start = block.checked_mul(size)?;
    let end = block.checked_add(count)?.checked_mul(size)?.min(base_len);
    Some(end - start)
}

/// Writes what `op` stands for to `out`: the bytes of the blocks it copies from `base`, or its literal.
/// Returns how many bytes were written. `base` is read through its own buffer by the caller if it wants one.
///
/// # Errors
///
/// [`ApplyError::PastEnd`] for a copy that names blocks `base` does not have; [`ApplyError::Io`].
pub fn apply(
    base: &mut (impl Read + Seek),
    op: &Op,
    block_size: u32,
    out: &mut impl Write,
) -> Result<u64, ApplyError> {
    match op {
        Op::Literal(bytes) => {
            out.write_all(bytes)?;
            Ok(bytes.len() as u64)
        }
        Op::Copy { block, count } => {
            let base_len = base.seek(SeekFrom::End(0))?;
            let length =
                copy_len(base_len, block_size, *block, *count).ok_or(ApplyError::PastEnd)?;
            base.seek(SeekFrom::Start(block * u64::from(block_size)))?;
            let mut buffer =
                vec![0_u8; usize::try_from(length).map_or(64 * 1024, |n| n.min(64 * 1024))];
            let mut left = length;
            while left > 0 {
                let want = usize::try_from(left).map_or(buffer.len(), |n| n.min(buffer.len()));
                match base.read(&mut buffer[..want]) {
                    Ok(0) => return Err(ApplyError::Io(io::ErrorKind::UnexpectedEof)),
                    Ok(count) => {
                        out.write_all(&buffer[..count])?;
                        left -= count as u64;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(length)
        }
    }
}
