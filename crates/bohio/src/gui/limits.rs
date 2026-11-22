//! The rates a GUI stream is held to: events a second on one stream, snapshots a minute for one peer.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Events a second on one stream: a window of a second that starts at its first event and is counted whole, so a burst
/// that would pass the limit inside it is refused and nothing of it is applied.
#[derive(Debug)]
pub(crate) struct InputRate {
    limit: u32,
    started: Option<Instant>,
    used: u32,
}

impl InputRate {
    pub(crate) const fn new(limit: u32) -> Self {
        Self {
            limit,
            started: None,
            used: 0,
        }
    }

    /// Takes `cost` events from the current second; `false` (taking nothing) when that passes the limit.
    pub(crate) fn admit(&mut self, cost: u32, now: Instant) -> bool {
        match self.started {
            Some(start) if now.duration_since(start) < Duration::from_secs(1) => {}
            _ => {
                self.started = Some(now);
                self.used = 0;
            }
        }
        match self.used.checked_add(cost) {
            Some(total) if total <= self.limit => {
                self.used = total;
                true
            }
            _ => false,
        }
    }
}

/// Requests a minute for each peer, across its streams: the times of the last minute's requests.
#[derive(Debug)]
pub(crate) struct PerPeerRate {
    limit: u32,
    seen: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl PerPeerRate {
    pub(crate) fn new(limit: u32) -> Self {
        Self {
            limit,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Counts one request of `peer`; `false` (counting nothing) when it has made its limit in the last minute.
    pub(crate) fn admit(&self, peer: &str) -> bool {
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.retain(|_, times| {
            while times
                .front()
                .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        let times = seen.entry(peer.to_owned()).or_default();
        if times.len() >= self.limit as usize {
            return false;
        }
        times.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_past_the_limit_is_refused_whole_and_the_next_second_starts_over() {
        let mut rate = InputRate::new(10);
        let start = Instant::now();
        assert!(rate.admit(6, start));
        assert!(!rate.admit(5, start));
        assert!(rate.admit(4, start));
        assert!(!rate.admit(1, start));
        assert!(rate.admit(10, start + Duration::from_millis(1001)));
    }

    #[test]
    fn a_peer_holds_to_its_snapshots_a_minute_and_others_are_not_counted() {
        let rate = PerPeerRate::new(2);
        assert!(rate.admit("a"));
        assert!(rate.admit("a"));
        assert!(!rate.admit("a"));
        assert!(rate.admit("b"));
    }
}
