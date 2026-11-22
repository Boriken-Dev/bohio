//! rsync's weak checksum, rolled one byte at a time.

/// The weak checksum of a window of bytes, which moves one byte forward in constant time.
///
/// For a window `x[0..L]`: `a = sum(x[i]) mod 2^16` and `b = sum((L - i) * x[i]) mod 2^16`, and the
/// value is `a + 2^16 * b` (Tridgell and Mackerras, "The rsync algorithm", 1996; no offset is added
/// to the bytes). Rolling to `x[1..=L]` is `a' = a - x[0] + x[L]` and `b' = b - L * x[0] + a'`, all
/// mod 2^16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rolling {
    a: u32,
    b: u32,
    len: u32,
}

impl Rolling {
    /// The checksum of `window`, which is at most `u32::MAX` bytes.
    pub fn new(window: &[u8]) -> Self {
        let mut a = 0_u32;
        let mut b = 0_u32;
        for byte in window {
            a = a.wrapping_add(u32::from(*byte));
            b = b.wrapping_add(a);
        }
        Self {
            a: a & 0xffff,
            b: b & 0xffff,
            len: u32::try_from(window.len()).unwrap_or(u32::MAX),
        }
    }

    /// Moves the window one byte forward: `out` leaves at its start and `incoming` joins at its end.
    pub fn roll(&mut self, out: u8, incoming: u8) {
        self.a = self
            .a
            .wrapping_sub(u32::from(out))
            .wrapping_add(u32::from(incoming))
            & 0xffff;
        self.b = self
            .b
            .wrapping_sub(self.len.wrapping_mul(u32::from(out)))
            .wrapping_add(self.a)
            & 0xffff;
    }

    /// The checksum: `a` in the low 16 bits and `b` in the high 16.
    pub const fn value(&self) -> u32 {
        self.a | (self.b << 16)
    }
}
