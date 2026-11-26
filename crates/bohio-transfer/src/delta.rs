//! Finding what a new file shares with an old one: the delta of the new file against the old file's signature.

use crate::rolling::Rolling;
use crate::signature::{Signature, SpecError};

/// One step of rebuilding the new file from the old one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `count` consecutive blocks of the old file, from block `block`. The old file's last block may be
    /// shorter than the block size.
    Copy {
        /// The first block.
        block: u64,
        /// How many blocks, at least 1.
        count: u64,
    },
    /// Bytes the old file does not have.
    Literal(Vec<u8>),
}

/// Matches a new file, fed a piece at a time, against the signature of an old one.
///
/// It slides a window of one block over the new file, rolling the weak checksum a byte at a time and
/// confirming each weak match with the strong hash, so a block that moved (an insertion shifts everything
/// after it) is found wherever it now lies. Memory is the signature's table, a window of one block, the
/// pieces not yet matched and one literal of at most the frame size; never the file. Consecutive copies
/// are merged, and among equal blocks of the old file the one that continues the last copy is preferred.
#[derive(Debug)]
pub struct Matcher {
    block: usize,
    /// The number of every full block, ordered by weak value, then strong hash, then number: a window's
    /// candidates are found by two binary searches, however many blocks share a weak value.
    sorted: Vec<u32>,
    /// A bit set of the weak values' hashes: most windows never reach the table.
    filter: Vec<u64>,
    shift: u32,
    signature: Signature,
    /// The signature's last block when it is shorter than the others.
    short: Option<(u32, usize)>,
    frame: usize,
    buf: Vec<u8>,
    pos: usize,
    rolling: Option<Rolling>,
    literal: Vec<u8>,
    pending: Option<(u64, u64)>,
    /// The block after the last copy, while no literal came after it.
    follows: Option<u64>,
    out: Vec<Op>,
}

impl Matcher {
    /// A matcher against `signature`, which must hold every block of its file; literals are cut at `frame`
    /// bytes (at least 1).
    ///
    /// # Errors
    ///
    /// [`SpecError::Incomplete`] when blocks are missing, [`SpecError::TooMany`] for more than `u32::MAX`.
    pub fn new(signature: Signature, frame: usize) -> Result<Self, SpecError> {
        if !signature.is_complete() {
            return Err(SpecError::Incomplete);
        }
        let count = signature.blocks();
        if count > u64::from(u32::MAX) {
            return Err(SpecError::TooMany);
        }
        let block = signature.spec().block_size as usize;
        let last = signature.last_len() as usize;
        let short = (count > 0 && last < block).then(|| {
            let index = (count - 1) as usize;
            (signature.weak()[index], index)
        });
        let full = if short.is_some() { count - 1 } else { count } as usize;
        let mut sorted: Vec<u32> = (0..full as u32).collect();
        sorted.sort_unstable_by(|a, b| {
            let (a, b) = (*a as usize, *b as usize);
            (signature.weak()[a], signature.strong(a), a).cmp(&(
                signature.weak()[b],
                signature.strong(b),
                b,
            ))
        });
        // About sixteen bits per block keeps the filter's false positives near a sixteenth.
        let bits = (full.max(1) * 16).next_power_of_two().max(64);
        let mut filter = vec![0_u64; bits / 64];
        let shift = 32 - bits.trailing_zeros().min(32);
        for index in &sorted {
            let at = Self::slot(signature.weak()[*index as usize], shift);
            filter[at / 64] |= 1 << (at % 64);
        }
        Ok(Self {
            block,
            sorted,
            filter,
            shift,
            signature,
            short,
            frame: frame.max(1),
            buf: Vec::new(),
            pos: 0,
            rolling: None,
            literal: Vec::new(),
            pending: None,
            follows: None,
            out: Vec::new(),
        })
    }

    fn slot(weak: u32, shift: u32) -> usize {
        if shift >= 32 {
            0
        } else {
            (weak.wrapping_mul(0x9E37_79B1) >> shift) as usize
        }
    }

    /// Feeds the next bytes of the new file; returns the steps they settle.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Op> {
        if self.sorted.is_empty() && self.short.is_none() {
            // An empty or one-short-block old file: only the tail can match.
            self.buf.extend_from_slice(bytes);
            let keep = self.block.max(1);
            if self.buf.len() > keep {
                let cut = self.buf.len() - keep;
                let gone: Vec<u8> = self.buf.drain(..cut).collect();
                self.add_literal(&gone);
            }
            return std::mem::take(&mut self.out);
        }
        self.buf.extend_from_slice(bytes);
        self.scan();
        // Moving what is left to the front costs as much as what is left: only when it is the smaller half.
        if self.pos > 0 && self.pos >= self.buf.len() / 2 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        std::mem::take(&mut self.out)
    }

    /// The end of the new file: the remaining steps.
    pub fn finish(mut self) -> Vec<Op> {
        self.scan();
        let tail = self.buf.split_off(self.pos);
        self.buf.clear();
        let mut copied_last = false;
        if let Some((weak, index)) = self.short {
            let last = self.signature.last_len() as usize;
            if tail.len() >= last {
                let suffix = &tail[tail.len() - last..];
                if Rolling::new(suffix).value() == weak
                    && self.signature.spec().strong_of(suffix) == self.signature.strong(index)
                {
                    self.add_literal(&tail[..tail.len() - last]);
                    self.emit_copy(index as u64);
                    copied_last = true;
                }
            }
        }
        if !copied_last {
            self.add_literal(&tail);
        }
        self.flush_literal();
        self.flush_pending();
        self.out
    }

    fn scan(&mut self) {
        let block = self.block;
        loop {
            let available = self.buf.len() - self.pos;
            if available < block || self.sorted.is_empty() {
                break;
            }
            let mut rolling = match self.rolling {
                Some(rolling) => rolling,
                None => Rolling::new(&self.buf[self.pos..self.pos + block]),
            };
            let weak = rolling.value();
            if self.may_match(weak)
                && let Some(found) = self.find(weak)
            {
                self.emit_copy(found);
                self.pos += block;
                self.rolling = None;
                continue;
            }
            if available > block {
                let leaving = self.buf[self.pos];
                let joining = self.buf[self.pos + block];
                rolling.roll(leaving, joining);
                self.rolling = Some(rolling);
                self.literal.push(leaving);
                self.pos += 1;
                if self.literal.len() >= self.frame {
                    self.flush_literal();
                }
            } else {
                self.rolling = Some(rolling);
                break;
            }
        }
    }

    fn may_match(&self, weak: u32) -> bool {
        let at = Self::slot(weak, self.shift);
        self.filter[at / 64] >> (at % 64) & 1 == 1
    }

    /// The block of the old file the window at `pos` equals, by weak value and then strong hash.
    fn find(&self, weak: u32) -> Option<u64> {
        let weaks = self.signature.weak();
        let start = self
            .sorted
            .partition_point(|index| weaks[*index as usize] < weak);
        // The strong hash costs a block's worth of hashing: only for a window some block shares the weak value with.
        let first = *self.sorted.get(start)?;
        if weaks[first as usize] != weak {
            return None;
        }
        let window = &self.buf[self.pos..self.pos + self.block];
        let strong = self.signature.spec().strong_of(window);
        if let Some(next) = self.follows
            && let Ok(index) = usize::try_from(next)
            && index < self.sorted.len()
            && weaks[index] == weak
            && self.signature.strong(index) == strong
        {
            return Some(next);
        }
        let at = start
            + self.sorted[start..].partition_point(|index| {
                let index = *index as usize;
                (weaks[index], self.signature.strong(index)) < (weak, strong.as_slice())
            });
        let found = *self.sorted.get(at)? as usize;
        (weaks[found] == weak && self.signature.strong(found) == strong).then_some(found as u64)
    }

    fn add_literal(&mut self, bytes: &[u8]) {
        for piece in bytes.chunks(self.frame) {
            if self.literal.len() + piece.len() > self.frame {
                self.flush_literal();
            }
            self.literal.extend_from_slice(piece);
            if self.literal.len() >= self.frame {
                self.flush_literal();
            }
        }
    }

    fn flush_literal(&mut self) {
        if self.literal.is_empty() {
            return;
        }
        self.flush_pending();
        self.follows = None;
        let literal = std::mem::replace(
            &mut self.literal,
            Vec::with_capacity(self.frame.min(1 << 20)),
        );
        self.out.push(Op::Literal(literal));
    }

    fn flush_pending(&mut self) {
        if let Some((block, count)) = self.pending.take() {
            self.out.push(Op::Copy { block, count });
        }
    }

    fn emit_copy(&mut self, block: u64) {
        self.flush_literal();
        match &mut self.pending {
            Some((start, count)) if *start + *count == block => *count += 1,
            _ => {
                self.flush_pending();
                self.pending = Some((block, 1));
            }
        }
        self.follows = Some(block + 1);
    }
}
