//! A counted bound on concurrent work, taken as a permit that frees itself.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// At most `limit` permits out at once, and when a share is set at most that many for one key.
#[derive(Debug)]
pub struct Slots {
    limit: usize,
    used: AtomicUsize,
    share: Option<(usize, Mutex<HashMap<String, usize>>)>,
}

impl Slots {
    /// Slots for `limit` concurrent holders.
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
            share: None,
        }
    }

    /// Slots for `limit` concurrent holders, of which one key (a peer) may hold at most `share`.
    pub fn with_share(limit: usize, share: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
            share: Some((share, Mutex::new(HashMap::new()))),
        }
    }

    /// As [`Slots::acquire`], for `key`: `None` also when `key` already holds its share.
    pub fn acquire_for(&self, key: &str) -> Option<Permit<'_>> {
        let Some((share, held)) = &self.share else {
            return self.acquire();
        };
        let mut held = held.lock().unwrap_or_else(PoisonError::into_inner);
        let count = held.get(key).copied().unwrap_or(0);
        if count >= *share {
            return None;
        }
        let mut permit = self.acquire()?;
        held.insert(key.to_owned(), count + 1);
        permit.key = Some(key.to_owned());
        Some(permit)
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
                Ok(_) => {
                    return Some(Permit {
                        slots: self,
                        key: None,
                    });
                }
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
    key: Option<String>,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let (Some(key), Some((_, held))) = (&self.key, &self.slots.share) {
            let mut held = held.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(count) = held.get_mut(key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    held.remove(key);
                }
            }
        }
        self.slots.used.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How many of something each key holds, at most `limit` at once.
#[derive(Debug)]
pub struct Counted<K> {
    held: Mutex<HashMap<K, usize>>,
    limit: usize,
}

impl<K: Eq + Hash + Clone> Counted<K> {
    /// Holds of at most `limit` for each key.
    pub fn new(limit: usize) -> Self {
        Self {
            held: Mutex::new(HashMap::new()),
            limit,
        }
    }

    /// One more for `key`, given back when the hold drops; `None` when `key` holds `limit`.
    pub fn take(&self, key: K) -> Option<Hold<'_, K>> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let count = held.entry(key.clone()).or_insert(0);
        if *count >= self.limit {
            return None;
        }
        *count += 1;
        Some(Hold { counted: self, key })
    }
}

/// One of a key's [`Counted`] holds.
#[derive(Debug)]
#[must_use = "the hold is given back when it drops"]
pub struct Hold<'a, K: Eq + Hash> {
    counted: &'a Counted<K>,
    key: K,
}

impl<K: Eq + Hash> Drop for Hold<'_, K> {
    fn drop(&mut self) {
        let mut held = self
            .counted
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = held.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                held.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Slots;

    #[test]
    fn a_key_holds_at_most_its_share_and_gives_it_back() {
        let slots = Slots::with_share(4, 2);
        let a1 = slots.acquire_for("a").expect("a first");
        let _a2 = slots.acquire_for("a").expect("a second");
        assert!(slots.acquire_for("a").is_none(), "a third of one key");
        let _b = slots.acquire_for("b").expect("another key");
        drop(a1);
        let _a3 = slots
            .acquire_for("a")
            .expect("a again after one was given back");
        // The total still holds: 2 of a, 1 of b, and one more of b is the fourth.
        let _b2 = slots.acquire_for("b").expect("the fourth slot");
        assert!(slots.acquire_for("c").is_none(), "past the total");
    }
}
