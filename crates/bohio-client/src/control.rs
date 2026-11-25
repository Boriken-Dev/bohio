//! The control stream: `bohio/1`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bohio_proto::control::{Call, Reply, Request, Response};
use bohio_proto::info::Info;
use bohio_proto::{decode, encode, protocol};
use coqui_transport::Channel;

use crate::{Error, Open};

/// What is outstanding on the stream.
#[derive(Default)]
struct Pending {
    /// The ids sent whose answer has not arrived, oldest first: the host answers in this order.
    awaiting: BTreeSet<u64>,
    /// Answers that arrived before the call that wants them collected them.
    answers: HashMap<u64, Response>,
    /// Ids whose caller stopped waiting: their answers are dropped when they come.
    abandoned: HashSet<u64>,
    /// A thread is reading the channel.
    reading: bool,
    /// The host closed the stream.
    closed: bool,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An open control stream. The host answers calls in the order it receives them, and every method takes `&self`: a
/// call from another thread joins the stream at once, and [`Control::submit`] lets one thread have many calls on the
/// way, so a path with a long round trip costs one round trip for the lot instead of one each.
pub struct Control {
    channel: Box<dyn Channel>,
    next_id: AtomicU64,
    /// Held while a request is sent, so ids reach the host in the order they were taken and `awaiting` is its answer order.
    send: Mutex<()>,
    pending: Mutex<Pending>,
    arrived: Condvar,
    /// The files source every call names, when the link names one.
    source: Option<String>,
}

impl core::fmt::Debug for Control {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

impl Control {
    /// Opens a `bohio/1` stream on `link`.
    ///
    /// # Errors
    ///
    /// Whatever `link` reports.
    pub fn open(link: &dyn Open) -> Result<Self, Error> {
        Ok(Self {
            channel: link.open(protocol::CONTROL, &[])?,
            next_id: AtomicU64::new(1),
            send: Mutex::new(()),
            pending: Mutex::new(Pending::default()),
            arrived: Condvar::new(),
            source: link.files_source().map(str::to_owned),
        })
    }

    /// Whether the stream has ended: closed here, or by the host.
    pub fn is_closed(&self) -> bool {
        self.channel.is_closed() || locked(&self.pending).closed
    }

    /// Sends `call` without waiting for its answer and returns its id, for [`Control::collect`]. Waits up to `timeout`
    /// (`None`: without limit) for room to send: a stream holds only its window of unanswered bytes.
    ///
    /// Many calls may be on the way at once. Answers are held for their own `collect`, so what memory pays for is the
    /// calls submitted and not collected; a thread that submits more than the stream's window carries must be collecting
    /// at the same time (see [`Control::call_many`], which does).
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] when there was no room in time, [`Error::Transport`] or [`Error::Closed`] otherwise.
    pub fn submit(&self, call: Call, timeout: Option<Duration>) -> Result<u64, Error> {
        let _send = locked(&self.send);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = encode(&Request {
            id,
            call,
            source: self.source.clone(),
        })?;
        locked(&self.pending).awaiting.insert(id);
        if let Err(failure) = self.channel.send_timeout(&request, timeout) {
            locked(&self.pending).awaiting.remove(&id);
            return Err(failure.into());
        }
        Ok(id)
    }

    /// Waits up to `timeout` (`None`: without limit) for the answer to the call [`Control::submit`] returned `id` for.
    /// Answers that arrive meanwhile are kept for their own `collect`. A timeout leaves the call outstanding: collect it
    /// again, or [`Control::abandon`] it.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host refused or failed the call, [`Error::Timeout`] when no answer came in time,
    /// [`Error::Closed`] when the host closed the stream, [`Error::Transport`] or [`Error::Protocol`] otherwise; and
    /// [`Error::Protocol`] for an id with nothing outstanding (already collected or abandoned).
    pub fn collect(&self, id: u64, timeout: Option<Duration>) -> Result<Reply, Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        self.collect_by(id, deadline)
    }

    fn collect_by(&self, id: u64, deadline: Option<Instant>) -> Result<Reply, Error> {
        let mut pending = locked(&self.pending);
        loop {
            if let Some(response) = pending.answers.remove(&id) {
                return response.into_result().map_err(Error::Remote);
            }
            if pending.closed {
                return Err(Error::Closed);
            }
            if !pending.awaiting.contains(&id) {
                return Err(Error::Protocol(bohio_proto::Error::Decode));
            }
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            if left.is_some_and(|left| left.is_zero()) {
                return Err(Error::Timeout);
            }
            if pending.reading {
                pending = match left {
                    Some(left) => {
                        self.arrived
                            .wait_timeout(pending, left)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0
                    }
                    None => self
                        .arrived
                        .wait(pending)
                        .unwrap_or_else(PoisonError::into_inner),
                };
                continue;
            }
            pending.reading = true;
            drop(pending);
            let frame = self.channel.receive(left);
            pending = locked(&self.pending);
            pending.reading = false;
            self.arrived.notify_all();
            match frame {
                Err(failure) => return Err(failure.into()),
                Ok(None) => {
                    pending.closed = true;
                    return Err(Error::Closed);
                }
                Ok(Some(frame)) => {
                    let response: Response = decode(&frame)?;
                    Self::deliver(&mut pending, response);
                }
            }
        }
    }

    /// Files an answer under its call. Id `0` answers a request the host could not read: the oldest one outstanding.
    fn deliver(pending: &mut Pending, mut response: Response) {
        let id = if response.id == 0 {
            match pending.awaiting.pop_first() {
                Some(id) => id,
                None => return,
            }
        } else {
            pending.awaiting.remove(&response.id);
            response.id
        };
        if !pending.abandoned.remove(&id) {
            response.id = id;
            pending.answers.insert(id, response);
        }
    }

    /// Gives up the call `id`: its answer is dropped when it arrives, or now if it already did. After a
    /// [`Error::Timeout`] from [`Control::collect`] this is how a caller that will not wait again frees what the call
    /// holds.
    pub fn abandon(&self, id: u64) {
        let mut pending = locked(&self.pending);
        if pending.answers.remove(&id).is_none() && pending.awaiting.contains(&id) {
            pending.abandoned.insert(id);
        }
    }

    /// Sends `call` and waits up to `timeout` (`None`: without limit) for its answer.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host refused or failed the call, [`Error::Timeout`] when no answer came in time
    /// (the stream goes on; a late answer is dropped), [`Error::Closed`] when the host closed the stream,
    /// [`Error::Transport`] or [`Error::Protocol`] otherwise.
    pub fn call(&self, call: Call, timeout: Option<Duration>) -> Result<Reply, Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let id = self.submit(call, timeout)?;
        self.finish(id, deadline)
    }

    /// Collects `id` by `deadline`, abandoning it on a timeout.
    fn finish(&self, id: u64, deadline: Option<Instant>) -> Result<Reply, Error> {
        match self.collect_by(id, deadline) {
            Err(Error::Timeout) => {
                self.abandon(id);
                Err(Error::Timeout)
            }
            other => other,
        }
    }

    /// Sends every call in `calls` and returns their answers in the same order, each its own result: a call the host
    /// refused does not fail the others. Sending and reading run on two threads, so a batch larger than the stream's
    /// window cannot deadlock; the batch costs about one round trip plus the host's time for each call.
    ///
    /// `timeout` bounds each wait, for room to send a call and for the next answer to arrive. A call the host does not
    /// answer in time reports [`Error::Timeout`] and its late answer is dropped; the host answers in order, so the calls
    /// behind it are as late and each waits its own `timeout` too, and the stream goes on.
    pub fn call_many(
        &self,
        calls: Vec<Call>,
        timeout: Option<Duration>,
    ) -> Vec<Result<Reply, Error>> {
        let count = calls.len();
        let (sent, ids) = mpsc::channel();
        let mut answers: Vec<Option<Result<Reply, Error>>> = (0..count).map(|_| None).collect();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                for (index, call) in calls.into_iter().enumerate() {
                    if sent.send((index, self.submit(call, timeout))).is_err() {
                        return;
                    }
                }
            });
            for (index, submitted) in ids {
                let answer = match submitted {
                    Ok(id) => self.finish(id, timeout.map(|limit| Instant::now() + limit)),
                    Err(error) => Err(error),
                };
                answers[index] = Some(answer);
            }
        });
        answers
            .into_iter()
            .map(|answer| answer.unwrap_or(Err(Error::Closed)))
            .collect()
    }

    /// Announces this client and returns the protocol version the host will speak.
    ///
    /// # Errors
    ///
    /// As [`Control::call`]; a host that answers anything but a hello is [`Error::Protocol`].
    pub fn hello(&self, client: &str, timeout: Option<Duration>) -> Result<u32, Error> {
        match self.call(
            Call::Hello {
                version: bohio_proto::PROTOCOL_VERSION,
                client: client.to_owned(),
            },
            timeout,
        )? {
            Reply::Hello { version, .. } => Ok(version),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// What the host offers for transfers (hashes, compression, deltas), from the `hello` reply: a peer needs no `info`
    /// permission for it.
    ///
    /// # Errors
    ///
    /// As [`Control::call`]; a host that answers anything but a hello is [`Error::Protocol`].
    pub fn transfer(
        &self,
        timeout: Option<Duration>,
    ) -> Result<bohio_proto::info::TransferInfo, Error> {
        match self.call(
            Call::Hello {
                version: bohio_proto::PROTOCOL_VERSION,
                client: "bohio-client".to_owned(),
            },
            timeout,
        )? {
            Reply::Hello { transfer, .. } => Ok(transfer),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// The host's capabilities.
    ///
    /// # Errors
    ///
    /// As [`Control::call`]; `denied` without the `info` permission.
    pub fn info(&self, timeout: Option<Duration>) -> Result<Info, Error> {
        match self.call(Call::Info, timeout)? {
            Reply::Info(info) => Ok(*info),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// What the host is running on: name, uptime, processors, load, memory and disks.
    ///
    /// # Errors
    ///
    /// As [`Control::call`]; `denied` without the `info` permission.
    pub fn stats(&self, timeout: Option<Duration>) -> Result<bohio_proto::control::Stats, Error> {
        match self.call(Call::Stats, timeout)? {
            Reply::Stats(stats) => Ok(*stats),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// Where each of `names` is on the host's own `PATH`, as an `exec` of a bare name finds it: the absolute path, or
    /// `None` for a name not found (also a name that holds a directory, which is not looked up).
    ///
    /// # Errors
    ///
    /// As [`Control::call`]; `denied` without the `info` permission, `bad_request` for no names.
    pub fn which(
        &self,
        names: &[&str],
        timeout: Option<Duration>,
    ) -> Result<std::collections::BTreeMap<String, Option<String>>, Error> {
        let names = names.iter().map(|name| (*name).to_owned()).collect();
        match self.call(Call::Which { names }, timeout)? {
            Reply::Which { found } => Ok(found),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// The detached sessions this peer holds (an `admin` sees every peer's, each with its `peer`), oldest first.
    ///
    /// # Errors
    ///
    /// As [`Control::call`]; `denied` without the `session` permission, `unsupported` on a host without sessions.
    pub fn sessions(
        &self,
        timeout: Option<Duration>,
    ) -> Result<Vec<bohio_proto::session::Listed>, Error> {
        match self.call(Call::Sessions, timeout)? {
            Reply::Sessions { sessions } => Ok(sessions),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// Ends the detached session `id` with `signal` (`kill` when `None`); the session then reports its exit and is kept for
    /// reading until the host's `keep_exited`.
    ///
    /// # Errors
    ///
    /// As [`Control::call`]; `not_found` for a session this peer does not hold.
    pub fn end_session(
        &self,
        id: &str,
        signal: Option<bohio_proto::signal::Signal>,
        timeout: Option<Duration>,
    ) -> Result<(), Error> {
        match self.call(
            Call::EndSession {
                id: id.to_owned(),
                signal,
            },
            timeout,
        )? {
            Reply::Done => Ok(()),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// Closes the stream.
    pub fn close(&self) {
        self.channel.close();
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.channel.close();
    }
}
