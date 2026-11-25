//! Keeping a link: waiting for one, and making a new one after it was lost.
//!
//! A forward that must survive a lost link holds a [`Links`] (whichever link is up now) instead of one link. [`Relink`] is the
//! [`Open`] over it: an open waits up to a time for a link to be up, so a connection that arrives during an outage is
//! served when the link returns. [`Reconnecting`] is a [`Links`] that makes a new link, with a [`Backoff`], each time
//! the last one ends.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cabuya::mux::LinkStream;

use crate::connect::Cause;
use crate::{Error, Open, OpenOptions};

/// How long to wait before the next attempt, as a failure count grows: `min` times `factor` for each failure in a row, up
/// to `max`, moved by up to `jitter` of itself so many clients do not retry together.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Backoff {
    /// The first wait. Default 1 second.
    pub min: Duration,
    /// The longest wait. Default 60 seconds.
    pub max: Duration,
    /// What each further failure multiplies the wait by; 1 is a fixed wait. Default 2.
    pub factor: f64,
    /// How far a wait may move, as a fraction of itself, from 0 to 1. Default 0.1.
    pub jitter: f64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            min: Duration::from_secs(1),
            max: Duration::from_secs(60),
            factor: 2.0,
            jitter: 0.1,
        }
    }
}

impl Backoff {
    /// How long to wait after `failures` failures in a row, moved in the direction `random` (from -1 to 1, see
    /// [`Jitter::sample`]) says.
    #[must_use]
    pub fn wait(&self, failures: u32, random: f64) -> Duration {
        let min = self.min.as_secs_f64();
        let max = self.max.as_secs_f64();
        let grown = min
            * self
                .factor
                .powi(i32::try_from(failures).unwrap_or(i32::MAX));
        let base = if grown.is_finite() {
            grown.min(max)
        } else {
            max
        };
        let moved = base * (1.0 + self.jitter * random);
        Duration::from_secs_f64(moved.max(0.001))
    }
}

/// Numbers from -1 to 1 for jitter, seeded by the process's own hasher randomness: nothing depends on them being
/// unpredictable, only on different clients not retrying in step.
#[derive(Debug)]
pub struct Jitter(u64);

impl Default for Jitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Jitter {
    /// A new sequence.
    #[must_use]
    pub fn new() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let seed = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        Self(seed | 1)
    }

    /// The next number, from -1 to 1.
    pub fn sample(&mut self) -> f64 {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        #[allow(
            clippy::cast_precision_loss,
            reason = "a fraction of 2^53 steps is all jitter needs"
        )]
        let unit = (self.0 >> 11) as f64 / (1u64 << 53) as f64;
        unit.mul_add(2.0, -1.0)
    }
}

/// The link to use now, whichever it is: a function from nothing to an up link, or `None` while there is none.
pub trait Links: Send + Sync {
    /// The link to open streams on, when one is up.
    fn current(&self) -> Option<Arc<dyn Open>>;
}

impl<F> Links for F
where
    F: Fn() -> Option<Arc<dyn Open>> + Send + Sync,
{
    fn current(&self) -> Option<Arc<dyn Open>> {
        self()
    }
}

/// An [`Open`] over [`Links`]: an open uses the link that is up now and, when there is none or it ended, waits up to `wait`
/// for one. After that it fails with [`Error::Link`], so what was waiting (a connection a local listener took during an outage)
/// is refused rather than kept for ever. [`Open::is_up`] is always true: a kept link is never over.
pub struct Relink {
    links: Arc<dyn Links>,
    wait: Duration,
    poll: Duration,
}

impl core::fmt::Debug for Relink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Relink")
            .field("wait", &self.wait)
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

impl Relink {
    /// Opens on `links`, waiting up to `wait` for one and looking again every `poll`.
    #[must_use]
    pub fn new(links: Arc<dyn Links>, wait: Duration, poll: Duration) -> Self {
        Self { links, wait, poll }
    }

    fn attempt<T>(&self, mut open: impl FnMut(&dyn Open) -> Result<T, Error>) -> Result<T, Error> {
        let until = Instant::now() + self.wait;
        loop {
            if let Some(link) = self.links.current() {
                match open(&*link) {
                    Ok(opened) => return Ok(opened),
                    // The link ended between asking for it and opening on it: the next one is waited for.
                    Err(Error::Closed | Error::Transport(coqui_transport::Failure::PathLost)) => {}
                    Err(other) => return Err(other),
                }
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Link(format!(
                    "the link is down and did not come back within {:?}",
                    self.wait
                )));
            }
            thread::sleep(self.poll.min(left));
        }
    }
}

impl Open for Relink {
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        self.attempt(|link| link.open(protocol, metadata))
    }

    fn open_with(
        &self,
        protocol: &str,
        metadata: &[u8],
        options: &OpenOptions,
    ) -> Result<Box<dyn LinkStream>, Error> {
        self.attempt(|link| link.open_with(protocol, metadata, options))
    }

    fn open_cause(
        &self,
        protocol: &str,
        metadata: &[u8],
    ) -> Result<(Box<dyn LinkStream>, Cause), Error> {
        self.attempt(|link| link.open_cause(protocol, metadata))
    }

    fn bulk_window(&self) -> Option<u32> {
        self.links.current().and_then(|link| link.bulk_window())
    }

    fn relink_wait(&self) -> Option<Duration> {
        Some(self.wait)
    }
}

/// What a [`Reconnecting`] link tells while it keeps the link.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kept {
    /// The link ended; a new one will be made.
    Lost,
    /// An attempt to make a new link failed, for this reason; the next is after `retry_in`.
    Failed {
        /// Why the attempt failed.
        reason: String,
        /// How long until the next attempt.
        retry_in: Duration,
    },
    /// A new link is up.
    Restored,
}

/// How a [`Reconnecting`] makes a link: whatever dials and proves one.
pub type Make = Box<dyn Fn() -> Result<Arc<dyn Open>, String> + Send + Sync>;

/// A link that is made again when it ends: [`Links`] over the one it holds. A thread of its own watches the link and, once
/// it is down, tries `make` after each [`Backoff`] wait until one is up. Dropping it stops the thread.
pub struct Reconnecting {
    shared: Arc<ReconnectShared>,
    thread: Option<JoinHandle<()>>,
}

struct ReconnectShared {
    current: Mutex<Option<Arc<dyn Open>>>,
    stop: AtomicBool,
}

impl core::fmt::Debug for Reconnecting {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Reconnecting")
            .field("up", &self.current().is_some())
            .finish_non_exhaustive()
    }
}

impl Reconnecting {
    /// Keeps `first`, a link already made, and makes the next with `make` when it ends. The thread looks at the link every
    /// `poll`; `report` hears what happens, on that thread.
    pub fn start(
        first: Arc<dyn Open>,
        make: Make,
        backoff: Backoff,
        poll: Duration,
        report: Box<dyn Fn(&Kept) + Send + Sync>,
    ) -> Self {
        let shared = Arc::new(ReconnectShared {
            current: Mutex::new(Some(first)),
            stop: AtomicBool::new(false),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || keep(&shared, &*make, &backoff, poll, &*report))
        };
        Self {
            shared,
            thread: Some(thread),
        }
    }

    /// The link, when one is up now.
    #[must_use]
    pub fn current(&self) -> Option<Arc<dyn Open>> {
        self.shared
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|link| link.is_up())
    }
}

impl Links for Reconnecting {
    fn current(&self) -> Option<Arc<dyn Open>> {
        Self::current(self)
    }
}

impl Drop for Reconnecting {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Sleeps `wait`, or until `stop`; `false` when it was stopped.
fn pause(stop: &AtomicBool, wait: Duration, poll: Duration) -> bool {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        thread::sleep(poll.min(until.saturating_duration_since(Instant::now())));
    }
    !stop.load(Ordering::SeqCst)
}

fn keep(
    shared: &ReconnectShared,
    make: &(dyn Fn() -> Result<Arc<dyn Open>, String> + Send + Sync),
    backoff: &Backoff,
    poll: Duration,
    report: &(dyn Fn(&Kept) + Send + Sync),
) {
    let mut jitter = Jitter::new();
    let mut failures = 0_u32;
    let mut up_since = Instant::now();
    while !shared.stop.load(Ordering::SeqCst) {
        let up = shared
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|link| link.is_up());
        if up {
            thread::sleep(poll);
            continue;
        }
        // A link that stayed up at least `min` starts the waits again; one that ended at once is a failure.
        if up_since.elapsed() >= backoff.min {
            failures = 0;
        }
        shared
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        report(&Kept::Lost);
        loop {
            let wait = backoff.wait(failures, jitter.sample());
            if !pause(&shared.stop, wait, poll) {
                return;
            }
            match make() {
                Ok(link) => {
                    *shared
                        .current
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some(link);
                    up_since = Instant::now();
                    report(&Kept::Restored);
                    break;
                }
                Err(reason) => {
                    failures = failures.saturating_add(1);
                    report(&Kept::Failed {
                        reason,
                        retry_in: backoff.wait(failures, 0.0),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link that opens nothing and says why.
    struct Refusing(Error);

    impl Open for Refusing {
        fn open(&self, _: &str, _: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
            Err(self.0.clone())
        }
    }

    #[test]
    fn the_wait_grows_by_the_factor_up_to_the_longest_and_jitter_moves_it() {
        let backoff = Backoff::default();
        assert_eq!(backoff.wait(0, 0.0), Duration::from_secs(1));
        assert_eq!(backoff.wait(3, 0.0), Duration::from_secs(8));
        assert_eq!(backoff.wait(20, 0.0), Duration::from_secs(60));
        assert_eq!(backoff.wait(0, 1.0), Duration::from_millis(1100));
        assert_eq!(backoff.wait(0, -1.0), Duration::from_millis(900));
        let mut jitter = Jitter::new();
        assert!((0..100).all(|_| (-1.0..=1.0).contains(&jitter.sample())));
    }

    #[test]
    fn an_open_with_no_link_waits_for_it_and_then_fails_naming_the_wait() {
        let none: Arc<dyn Links> = Arc::new(|| None::<Arc<dyn Open>>);
        let relink = Relink::new(none, Duration::from_millis(200), Duration::from_millis(10));
        let began = Instant::now();
        let error = relink.open("p", b"").err().expect("no link");
        assert!(matches!(error, Error::Link(_)), "{error:?}");
        assert!(began.elapsed() >= Duration::from_millis(200));
        assert_eq!(relink.relink_wait(), Some(Duration::from_millis(200)));
    }

    #[test]
    fn an_open_waits_for_a_link_that_comes_and_does_not_retry_a_refusal() {
        let appears = Instant::now() + Duration::from_millis(150);
        let links: Arc<dyn Links> = Arc::new(move || {
            (Instant::now() >= appears).then(|| Arc::new(Refusing(Error::Timeout)) as Arc<dyn Open>)
        });
        let relink = Relink::new(links, Duration::from_secs(10), Duration::from_millis(10));
        let began = Instant::now();
        let error = relink.open("p", b"").err().expect("refused");
        assert_eq!(error, Error::Timeout, "the link's own error is the answer");
        assert!(began.elapsed() >= Duration::from_millis(100));
        assert!(began.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_reconnecting_link_is_made_again_when_it_ends() {
        struct Flag(AtomicBool);
        impl Open for Flag {
            fn open(&self, _: &str, _: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
                Err(Error::Closed)
            }
            fn is_up(&self) -> bool {
                self.0.load(Ordering::SeqCst)
            }
        }
        let first = Arc::new(Flag(AtomicBool::new(true)));
        let made = Arc::new(Mutex::new(0_u32));
        let kept = {
            let made = Arc::clone(&made);
            Reconnecting::start(
                first.clone(),
                Box::new(move || {
                    *made.lock().unwrap() += 1;
                    Ok(Arc::new(Flag(AtomicBool::new(true))) as Arc<dyn Open>)
                }),
                Backoff {
                    min: Duration::from_millis(20),
                    max: Duration::from_millis(50),
                    ..Backoff::default()
                },
                Duration::from_millis(10),
                Box::new(|_| {}),
            )
        };
        assert!(kept.current().is_some());
        first.0.store(false, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(10);
        while *made.lock().unwrap() == 0 {
            assert!(Instant::now() < until, "a new link was made");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(kept.current().is_some_and(|link| link.is_up()));
    }
}
