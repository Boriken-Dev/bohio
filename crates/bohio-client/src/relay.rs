//! A bounded, owned channel in front of a link's stream.
//!
//! A stream of a link borrows the link, so it cannot be handed to code that wants an owned,
//! `'static` channel (an engine that starts threads, a client that outlives the call that opened
//! the stream). [`relay`] puts two queues and two threads between them: what the stream receives
//! is queued for the relay's reader, and what the relay's writer sends is queued for the stream.
//! Each queue holds at most [`Settings::queue_bytes`](crate::link::Settings::queue_bytes) (a message
//! larger than that passes alone), so a slow reader or writer makes the other side wait instead of
//! growing a buffer.

use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::Scope;
use std::time::{Duration, Instant};

use coqui_link::mux::StreamEnd;
use coqui_transport::{Capabilities, Carrier, Channel, Failure};

use crate::link::Settings;

/// An owned channel over one stream of a link; see the [module](self).
///
/// Closing it, or dropping the last handle a client holds, makes its threads deliver what is
/// queued (for at most [`Settings::send_limit`](crate::link::Settings::send_limit)) and end.
#[derive(Debug)]
pub struct Relay {
    state: Mutex<State>,
    changed: Condvar,
    queue_bytes: usize,
    max_message: usize,
    give_up: Duration,
}

#[derive(Debug, Default)]
struct State {
    inbound: VecDeque<Vec<u8>>,
    inbound_bytes: usize,
    /// `Some(None)`: the stream ended in order; `Some(Some(failure))`: it failed or was closed
    /// by the host with a reason.
    inbound_end: Option<Option<Failure>>,
    /// Why the stream ended, when the stream said (a refusal or a revoke with its text).
    cause: Option<StreamEnd>,
    outbound: VecDeque<Vec<u8>>,
    outbound_bytes: usize,
    outbound_failed: Option<Failure>,
    closed: Option<Instant>,
}

/// What the writer thread finds next.
enum Next {
    Message(Vec<u8>),
    Idle,
    Done,
}

impl Relay {
    fn new(settings: &Settings) -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            queue_bytes: settings.queue_bytes.max(1),
            max_message: settings.max_message.max(1),
            give_up: settings.send_limit,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for a change, until `until` when given. The flag is `true` when `until` had already
    /// passed, so nothing was waited for.
    fn wait<'a>(
        &self,
        state: MutexGuard<'a, State>,
        until: Option<Instant>,
    ) -> (MutexGuard<'a, State>, bool) {
        let Some(until) = until else {
            let state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            return (state, false);
        };
        match until.checked_duration_since(Instant::now()) {
            Some(left) if !left.is_zero() => {
                let (state, _) = self
                    .changed
                    .wait_timeout(state, left)
                    .unwrap_or_else(PoisonError::into_inner);
                (state, false)
            }
            _ => (state, true),
        }
    }

    /// Queues one received message for the reader, waiting while the queue is full. `false`
    /// when the relay was closed meanwhile.
    fn push_inbound(&self, message: Vec<u8>) -> bool {
        let mut state = self.lock();
        loop {
            if state.closed.is_some() {
                return false;
            }
            if state.inbound_bytes == 0 || state.inbound_bytes + message.len() <= self.queue_bytes {
                state.inbound_bytes += message.len();
                state.inbound.push_back(message);
                self.changed.notify_all();
                return true;
            }
            state = self.wait(state, None).0;
        }
    }

    fn end_inbound(&self, failure: Option<Failure>, cause: Option<StreamEnd>) {
        let mut state = self.lock();
        if state.inbound_end.is_none() {
            state.inbound_end = Some(failure);
            state.cause = cause;
        }
        self.changed.notify_all();
    }

    /// Why the stream ended, with the text its peer gave (a refusal, a revoke), once it has; `None`
    /// while it runs and for a stream that cannot say.
    pub fn end(&self) -> Option<StreamEnd> {
        self.lock().cause.clone()
    }

    fn next_outbound(&self, wait: Duration) -> Next {
        let mut state = self.lock();
        if let Some(message) = state.outbound.pop_front() {
            state.outbound_bytes -= message.len();
            self.changed.notify_all();
            return Next::Message(message);
        }
        if state.closed.is_some() || state.inbound_end.is_some() {
            return Next::Done;
        }
        drop(self.wait(state, Some(Instant::now() + wait)));
        Next::Idle
    }

    fn fail_outbound(&self, failure: Failure) {
        let mut state = self.lock();
        state.outbound_failed.get_or_insert(failure);
        state.outbound.clear();
        state.outbound_bytes = 0;
        self.changed.notify_all();
    }

    /// Whether the writer should stop trying to deliver what is queued: the relay was closed
    /// longer ago than the give-up time.
    fn out_of_time(&self) -> bool {
        self.lock()
            .closed
            .is_some_and(|closed| closed.elapsed() >= self.give_up)
    }

    /// Why nothing more can be sent: the failure the stream ended with (a refusal, a revoke), else
    /// that it is gone.
    fn reason_gone(&self) -> Failure {
        self.lock()
            .inbound_end
            .flatten()
            .unwrap_or(Failure::PathLost)
    }

    fn is_closed_by_user(&self) -> bool {
        self.lock().closed.is_some()
    }
}

impl Channel for Relay {
    fn send(&self, message: &[u8]) -> Result<(), Failure> {
        self.send_timeout(message, None)
    }

    fn send_timeout(&self, message: &[u8], timeout: Option<Duration>) -> Result<(), Failure> {
        if message.len() > self.max_message {
            return Err(Failure::Protocol);
        }
        let until = timeout.map(|limit| Instant::now() + limit);
        let mut state = self.lock();
        loop {
            if state.closed.is_some() {
                return Err(Failure::PathLost);
            }
            if let Some(failure) = state.outbound_failed {
                return Err(failure);
            }
            if state.outbound_bytes == 0 || state.outbound_bytes + message.len() <= self.queue_bytes
            {
                state.outbound_bytes += message.len();
                state.outbound.push_back(message.to_vec());
                self.changed.notify_all();
                return Ok(());
            }
            let (next, expired) = self.wait(state, until);
            if expired {
                return Err(Failure::Timeout);
            }
            state = next;
        }
    }

    fn receive(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, Failure> {
        let until = timeout.map(|limit| Instant::now() + limit);
        let mut state = self.lock();
        loop {
            if let Some(message) = state.inbound.pop_front() {
                state.inbound_bytes -= message.len();
                self.changed.notify_all();
                return Ok(Some(message));
            }
            if let Some(end) = state.inbound_end {
                return end.map_or(Ok(None), Err);
            }
            if state.closed.is_some() {
                return Ok(None);
            }
            let (next, expired) = self.wait(state, until);
            if expired {
                return Err(Failure::Timeout);
            }
            state = next;
        }
    }

    fn close(&self) {
        let mut state = self.lock();
        state.closed.get_or_insert_with(Instant::now);
        self.changed.notify_all();
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::new()
            .carrier(Carrier::Messages)
            .limits(self.max_message, self.max_message)
    }

    fn is_closed(&self) -> bool {
        let state = self.lock();
        state.closed.is_some() || state.inbound_end.is_some() || state.outbound_failed.is_some()
    }
}

/// Starts the two threads that connect `stream` to a new [`Relay`], in `scope`, and returns the
/// relay. The threads end once the relay is closed and what it holds is delivered, or the stream
/// ends; the caller closes the relay before the scope ends.
///
/// `stream` is anything that derefs to a channel and can be handed to two threads: a reference
/// to a stream (`&dyn Channel`) or an `Arc` of one. A stream the host refused, revoked or lost
/// surfaces from [`Relay::receive`] as the failure it reported (`Permission` for a refusal or a
/// revoke).
pub fn relay<'scope, 'env, P>(
    scope: &'scope Scope<'scope, 'env>,
    stream: P,
    settings: &Settings,
) -> Arc<Relay>
where
    P: Deref + Clone + Send + 'scope,
    P::Target: Channel,
{
    relay_with(scope, stream, settings, |_| None)
}

/// [`relay`], asking `ended` for the stream's own account of why it ended (a refusal or a revoke with
/// its text), which [`Relay::end`] then gives.
pub fn relay_with<'scope, 'env, P, E>(
    scope: &'scope Scope<'scope, 'env>,
    stream: P,
    settings: &Settings,
    ended: E,
) -> Arc<Relay>
where
    P: Deref + Clone + Send + 'scope,
    P::Target: Channel,
    E: Fn(&P::Target) -> Option<StreamEnd> + Send + 'scope,
{
    let relay = Arc::new(Relay::new(settings));
    let poll = settings.poll_interval.max(Duration::from_millis(1));
    {
        let (relay, stream) = (Arc::clone(&relay), stream.clone());
        scope.spawn(move || read_side(&relay, &*stream, poll, ended));
    }
    {
        let relay = Arc::clone(&relay);
        scope.spawn(move || write_side(&relay, &*stream, poll));
    }
    relay
}

/// Moves what the stream receives into the relay.
fn read_side<C: Channel + ?Sized>(
    relay: &Relay,
    stream: &C,
    poll: Duration,
    ended: impl Fn(&C) -> Option<StreamEnd>,
) {
    loop {
        if relay.is_closed_by_user() {
            return;
        }
        match stream.receive(Some(poll)) {
            Ok(Some(message)) => {
                if !relay.push_inbound(message) {
                    return;
                }
            }
            Ok(None) => return relay.end_inbound(None, ended(stream)),
            Err(Failure::Timeout) => {}
            Err(failure) => return relay.end_inbound(Some(failure), ended(stream)),
        }
    }
}

/// Moves what the relay's writer queued onto the stream.
fn write_side<C: Channel + ?Sized>(relay: &Relay, stream: &C, poll: Duration) {
    loop {
        match relay.next_outbound(poll) {
            Next::Message(message) => loop {
                match stream.send(&message) {
                    Ok(()) => break,
                    Err(Failure::NoProgress | Failure::Timeout) => {
                        if relay.out_of_time() {
                            return relay.fail_outbound(Failure::NoProgress);
                        }
                    }
                    Err(failure) => return relay.fail_outbound(failure),
                }
            },
            Next::Idle => {}
            Next::Done => return relay.fail_outbound(relay.reason_gone()),
        }
    }
}
