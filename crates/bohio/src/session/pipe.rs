//! An in-process pair of channels: the session layer on one end and the exec or shell engine on the other.
//!
//! The engines serve a `Channel` and end their program's tree when it closes. A detached session is the
//! same engine served on a channel nobody closes: this pair is that channel, and what the session layer
//! does not read waits here, which is what stops a program whose output nobody has taken.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use coqui_transport::{Channel, Failure};

/// One direction: a bounded queue of messages.
struct Lane {
    state: Mutex<LaneState>,
    changed: Condvar,
    capacity: usize,
}

struct LaneState {
    queue: VecDeque<Vec<u8>>,
    /// The sender is finished: what is queued is still read, then the reader sees the end.
    closed: bool,
    /// The reader is gone: a send fails.
    unread: bool,
}

impl Lane {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(LaneState {
                queue: VecDeque::new(),
                closed: false,
                unread: false,
            }),
            changed: Condvar::new(),
            capacity: capacity.max(1),
        })
    }

    fn lock(&self) -> MutexGuard<'_, LaneState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One end of the pair.
pub(super) struct End {
    out: Arc<Lane>,
    input: Arc<Lane>,
}

/// Two connected ends; each direction holds at most `capacity` messages.
pub(super) fn pair(capacity: usize) -> (End, End) {
    let there = Lane::new(capacity);
    let back = Lane::new(capacity);
    (
        End {
            out: Arc::clone(&there),
            input: Arc::clone(&back),
        },
        End {
            out: back,
            input: there,
        },
    )
}

impl Channel for End {
    fn send(&self, message: &[u8]) -> Result<(), Failure> {
        self.send_timeout(message, Some(Duration::ZERO))
    }

    fn send_timeout(&self, message: &[u8], timeout: Option<Duration>) -> Result<(), Failure> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let mut state = self.out.lock();
        loop {
            if state.closed || state.unread {
                return Err(Failure::PathLost);
            }
            if state.queue.len() < self.out.capacity {
                state.queue.push_back(message.to_vec());
                self.out.changed.notify_all();
                return Ok(());
            }
            state = match deadline {
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(Failure::Timeout);
                    }
                    self.out
                        .changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .out
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }

    fn receive(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, Failure> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let mut state = self.input.lock();
        loop {
            if let Some(message) = state.queue.pop_front() {
                self.input.changed.notify_all();
                return Ok(Some(message));
            }
            if state.closed {
                return Ok(None);
            }
            state = match deadline {
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(Failure::Timeout);
                    }
                    self.input
                        .changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .input
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }

    fn close(&self) {
        {
            let mut out = self.out.lock();
            out.closed = true;
            self.out.changed.notify_all();
        }
        let mut input = self.input.lock();
        input.unread = true;
        self.input.changed.notify_all();
    }

    fn is_closed(&self) -> bool {
        let out = self.out.lock();
        if out.closed || out.unread {
            return true;
        }
        drop(out);
        let input = self.input.lock();
        input.closed && input.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_lane_makes_the_sender_wait_and_a_close_ends_the_reader_after_what_is_queued() {
        let (a, b) = pair(1);
        a.send(b"one").unwrap();
        assert_eq!(
            a.send_timeout(b"two", Some(Duration::from_millis(30))),
            Err(Failure::Timeout)
        );
        assert_eq!(
            b.receive(Some(Duration::from_secs(1))).unwrap().unwrap(),
            b"one"
        );
        a.send(b"two").unwrap();
        a.close();
        assert!(a.is_closed());
        assert_eq!(
            b.receive(Some(Duration::from_secs(1))).unwrap().unwrap(),
            b"two"
        );
        assert_eq!(b.receive(Some(Duration::from_secs(1))).unwrap(), None);
        assert_eq!(b.send(b"late"), Err(Failure::PathLost));
    }
}
