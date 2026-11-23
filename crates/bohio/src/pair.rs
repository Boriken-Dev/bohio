//! Pairing on a node's own listeners: one PIN armed at a time, in memory, tried once.
//!
//! A joiner reaches a node over any transport the node listens on and sends [`coqui_link::pairing::MARKER`] before its
//! handshake, which is how the node tells a pairing from a proof (a pairing and a proof begin with the same handshake message
//! under both methods). The node holds at most one armed PIN, made by [`Node::arm_pairing`] or by a client of the local
//! `bohio-pair/1` protocol. Unarmed, a joiner is refused as one with no PIN shown, and that is an event.
//!
//! A pairing writes nothing itself: the joiner's key is handed to [`Keys::paired`], which the node's owner implements (for
//! `bohio serve`, by writing the joiner's `[peer.<id>]` table, holding nothing).
//!
//! [`Node::arm_pairing`]: crate::node::Node::arm_pairing
//! [`Keys::paired`]: crate::link::Keys::paired

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use coqui_link::auth::prove;
use coqui_link::link::Prefilled;
use coqui_link::pairing::{self, PairingHost, respond, starts_pairing};
use coqui_peer::pair::{CodeAlphabet, DeskLimits, Paired, PairingDesk, PairingOptions};
use coqui_peer::{Established, Handshake, PeerId, TrustStore};
use coqui_transport::{Channel, Failure};

use crate::config::PairingConfig;
use crate::node::NodeError;

/// How an armed PIN ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PairEnd {
    /// A peer paired with it, and the node's keys recorded it.
    Paired(PeerId),
    /// Its lifetime passed.
    Expired,
    /// A guess was made and failed: a PIN is tried once.
    UsedUp,
    /// Another request armed a new PIN in its place.
    Replaced,
    /// Its owner withdrew it.
    Withdrawn,
    /// A peer paired with it, and the node's keys could not record that; the reason holds no key or PIN.
    Failed(String),
}

impl core::fmt::Display for PairEnd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Paired(peer) => write!(f, "paired {peer}"),
            Self::Expired => f.write_str("the PIN expired"),
            Self::UsedUp => f.write_str("the PIN was tried and used up"),
            Self::Replaced => f.write_str("another request armed a new PIN"),
            Self::Withdrawn => f.write_str("the PIN was withdrawn"),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}

#[derive(Default)]
struct SlotState {
    end: Option<PairEnd>,
    /// Attempts that are past the first message and not finished: the PIN is not used up while one may still succeed.
    running: usize,
    /// Failed attempts, in all and by source: how many, and when the source may try again.
    failed: u32,
    sources: HashMap<IpAddr, (u32, Instant)>,
}

/// One armed PIN.
pub(crate) struct Slot {
    pub(crate) desk: PairingDesk,
    lifetime: Duration,
    until: Instant,
    delay: Duration,
    delay_max: Duration,
    state: Mutex<SlotState>,
    changed: Condvar,
}

impl Slot {
    fn state(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends it as `end` unless it ended already.
    pub(crate) fn finish(&self, end: PairEnd) {
        let mut state = self.state();
        if state.end.is_none() {
            state.end = Some(end);
            self.desk.clear_pin();
        }
        self.changed.notify_all();
    }

    /// How it ended, once it has: a lifetime that passed with no attempt running ends it here.
    fn ended(&self, state: &mut SlotState) -> Option<PairEnd> {
        if state.end.is_none() && state.running == 0 && Instant::now() >= self.until {
            state.end = Some(PairEnd::Expired);
            self.desk.clear_pin();
        }
        state.end.clone()
    }

    /// A failed pairing attempt from `source`: it must now wait, doubling at each one up to the longest wait. Answers how
    /// many attempts have failed against this PIN.
    pub(crate) fn failed(&self, source: Option<IpAddr>) -> u32 {
        let mut state = self.state();
        state.failed = state.failed.saturating_add(1);
        if let Some(source) = source {
            let count = state.sources.get(&source).map_or(0, |(count, _)| *count);
            let wait = self
                .delay
                .saturating_mul(2_u32.saturating_pow(count.min(30)))
                .min(self.delay_max);
            state
                .sources
                .insert(source, (count.saturating_add(1), Instant::now() + wait));
        }
        state.failed
    }

    /// Whether `source` is still within the wait a wrong guess of its own earned.
    fn waiting(&self, source: Option<IpAddr>) -> bool {
        source.is_some_and(|source| {
            self.state()
                .sources
                .get(&source)
                .is_some_and(|(_, until)| Instant::now() < *until)
        })
    }

    /// Whether it still takes connections; one that begins is counted until [`Slot::attempted`], so the PIN is not
    /// reported used up while a connection that holds it may still pair.
    pub(crate) fn begin(&self) -> bool {
        let mut state = self.state();
        if self.ended(&mut state).is_some() {
            return false;
        }
        state.running += 1;
        true
    }

    /// An attempt that began is over, and `paired` when it ended in a pairing: the PIN is used up when it failed and
    /// the desk shows none any more.
    pub(crate) fn attempted(&self, paired: bool) {
        let mut state = self.state();
        state.running -= 1;
        if !paired && state.end.is_none() && state.running == 0 && self.desk.shown_pin().is_none() {
            state.end = Some(PairEnd::UsedUp);
        }
        self.changed.notify_all();
    }

    fn wait(&self, up_to: Duration) -> Option<PairEnd> {
        let mut state = self.state();
        if let Some(end) = self.ended(&mut state) {
            return Some(end);
        }
        let until = Instant::now() + up_to;
        loop {
            let now = Instant::now();
            let wake = if self.until > now {
                until.min(self.until)
            } else {
                until
            };
            let (next, _) = self
                .changed
                .wait_timeout(state, wake.saturating_duration_since(now))
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            if let Some(end) = self.ended(&mut state) {
                return Some(end);
            }
            if Instant::now() >= until {
                return None;
            }
        }
    }
}

/// The PIN a node armed. The PIN is for the person who reads it out: it is in no log or event, and not in this type's
/// `Debug`.
pub struct Armed {
    slot: Arc<Slot>,
    pin: String,
}

impl Armed {
    /// The PIN.
    pub fn pin(&self) -> &str {
        &self.pin
    }

    /// How long it lasts.
    pub fn lifetime(&self) -> Duration {
        self.slot.lifetime
    }

    /// Waits up to `up_to` for it to end; `None` while it has not.
    pub fn wait(&self, up_to: Duration) -> Option<PairEnd> {
        self.slot.wait(up_to)
    }

    /// Withdraws the PIN: a joiner is then refused as one with none shown.
    pub fn withdraw(&self) {
        self.slot.finish(PairEnd::Withdrawn);
    }
}

impl core::fmt::Debug for Armed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Armed")
            .field("lifetime", &self.slot.lifetime)
            .finish_non_exhaustive()
    }
}

/// What a listener's connection turned out to be.
pub(crate) enum Answered<T> {
    /// A pairing, complete: the joiner's key is accepted in the store.
    Paired(Paired),
    /// A proof, complete.
    Proven(Established<T>),
}

/// Why [`answer`] ended without an answer, and whether the connection was a pairing.
pub(crate) struct Unanswered {
    /// `true` when the joiner sent the pairing marker: a refused pairing, never a failed proof.
    pub(crate) pairing: bool,
    /// What went wrong.
    pub(crate) error: coqui_link::Error,
}

/// The responder of a listener that takes proofs and pairings: reads the first message and runs a pairing when it is the
/// marker, a proof otherwise (the message is not lost). `limit` bounds the whole run; a pairing is also held to
/// `pairing_limit` counted from the first message, so a joiner that goes silent after the marker frees its desk place
/// sooner than a proof would. With `pairing` `None` a joiner is sent away at once.
pub(crate) fn answer<M: coqui_peer::Method, C: Channel>(
    channel: &C,
    method: &M,
    max_message: usize,
    pairing: Option<PairingHost<'_>>,
    source: Option<SocketAddr>,
    limit: Duration,
    pairing_limit: Duration,
) -> Result<Answered<<M::Handshake as Handshake>::Output>, Unanswered> {
    let deadline = Instant::now() + limit;
    let proof = |error| Unanswered {
        pairing: false,
        error,
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(proof(coqui_link::Error::Channel(Failure::Timeout)));
    }
    let first = match channel.receive(Some(left)) {
        Ok(Some(message)) => message,
        Ok(None) => return Err(proof(coqui_link::Error::Channel(Failure::PathLost))),
        Err(failure) => return Err(proof(failure.into())),
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if starts_pairing(&first) {
        let refused = |error| Unanswered {
            pairing: true,
            error,
        };
        let Some(pairing) = pairing else {
            return Err(refused(coqui_link::Error::Proof(
                "pairing is not offered on this listener".to_owned(),
            )));
        };
        respond(
            channel,
            pairing.identity,
            pairing.store,
            pairing.desk,
            source,
            left.min(pairing_limit),
        )
        .map(Answered::Paired)
        .map_err(refused)
    } else {
        let channel = Prefilled::new(channel, first);
        prove(method, &channel, left, max_message)
            .map(Answered::Proven)
            .map_err(proof)
    }
}

struct Setup {
    store: TrustStore,
    /// A desk that never shows a PIN, so a joiner meets a refusal that says so.
    idle: PairingDesk,
}

/// What a node knows of pairing: the store a joiner's key is accepted in, and the PIN armed now.
pub(crate) struct PairState {
    setup: RwLock<Option<Setup>>,
    armed: Mutex<Option<Arc<Slot>>>,
    config: PairingConfig,
}

impl PairState {
    pub(crate) fn new(config: PairingConfig) -> Self {
        Self {
            setup: RwLock::new(None),
            armed: Mutex::new(None),
            config,
        }
    }

    pub(crate) fn enable(&self, store: TrustStore) -> Result<(), NodeError> {
        let idle = pairing::desk().map_err(|error| NodeError::Proof(error.to_string()))?;
        *self.setup.write().unwrap_or_else(PoisonError::into_inner) = Some(Setup { store, idle });
        Ok(())
    }

    pub(crate) fn arm(&self, lifetime: Option<Duration>) -> Result<Armed, NodeError> {
        if self
            .setup
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
        {
            return Err(NodeError::Pairing(
                "this node has no store to pair into".to_owned(),
            ));
        }
        let lifetime = lifetime.unwrap_or(self.config.timeout.0);
        if lifetime.is_zero() {
            return Err(NodeError::Pairing(
                "a PIN's lifetime must not be zero".to_owned(),
            ));
        }
        let mut options = PairingOptions::default();
        options.pin_ttl = Some(lifetime);
        options.pin_attempts = match self.config.max_attempts {
            0 => u32::MAX,
            attempts => attempts,
        };
        options.pin_alphabet = CodeAlphabet::new("0123456789", self.config.pin_length);
        let mut limits = DeskLimits::default();
        limits.pending = self.config.pending;
        limits.pending_per_source = self.config.pending_per_source;
        limits.ipv6_prefix = self.config.ipv6_prefix;
        // The wait after a wrong guess is this node's own, per source; the desk's runs per window would end a PIN early.
        limits.pin_runs_per_source = usize::MAX;
        let desk = pairing::desk_with(options, limits)
            .map_err(|error| NodeError::Pairing(error.to_string()))?;
        let pin = desk
            .show_pin()
            .map_err(|error| NodeError::Pairing(error.to_string()))?
            .pin
            .as_str()
            .to_owned();
        let slot = Arc::new(Slot {
            desk,
            lifetime,
            until: Instant::now() + lifetime,
            delay: self.config.attempt_delay.0,
            delay_max: self.config.attempt_delay_max.0,
            state: Mutex::new(SlotState::default()),
            changed: Condvar::new(),
        });
        let mut armed = self.armed.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = armed.replace(Arc::clone(&slot)) {
            old.finish(PairEnd::Replaced);
        }
        Ok(Armed { slot, pin })
    }

    /// The armed slot, if it still takes attempts and counts this one, with whether pairing is offered at all.
    pub(crate) fn host<R>(
        &self,
        identity: &coqui_link::auth::Identity,
        source: Option<IpAddr>,
        body: impl FnOnce(Option<PairingHost<'_>>, Option<&Slot>, bool) -> R,
    ) -> R {
        let setup = self.setup.read().unwrap_or_else(PoisonError::into_inner);
        let Some(setup) = setup.as_ref() else {
            return body(None, None, false);
        };
        let slot = self
            .armed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|slot| slot.begin());
        let waiting = slot.as_ref().is_some_and(|slot| slot.waiting(source));
        let desk = slot.as_ref().map_or(&setup.idle, |slot| &slot.desk);
        let host = PairingHost {
            identity,
            store: &setup.store,
            desk,
        };
        // A source that is waiting is refused without a try: no host is offered to it.
        body((!waiting).then_some(host), slot.as_deref(), waiting)
    }
}
