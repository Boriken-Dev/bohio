//! A counted bound on concurrent work, taken as a permit that frees itself.

use std::sync::atomic::{AtomicUsize, Ordering};

/// At most `limit` permits out at once.
#[derive(Debug)]
pub struct Slots {
    limit: usize,
    used: AtomicUsize,
}

impl Slots {
    /// Slots for `limit` concurrent holders.
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    /// Takes a slot, or `None` when all are taken.
    pub fn acquire(&self) -> Option<Permit<'_>> {
        let mut current = self.used.load(Ordering::Acquire);
        loop {
            if current >= self.limit {
                return None;
            }
            match self.used.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Permit { slots: self }),
                Err(actual) => current = actual,
            }
        }
    }

    /// How many permits are out.
    pub fn in_use(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    /// The bound.
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

/// A taken slot, returned when dropped.
#[derive(Debug)]
#[must_use = "the slot is returned when the permit drops"]
pub struct Permit<'a> {
    slots: &'a Slots,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.slots.used.fetch_sub(1, Ordering::AcqRel);
    }
}
