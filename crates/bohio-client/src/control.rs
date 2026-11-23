//! The control stream: `bohio/1`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bohio_proto::control::{Call, Reply, Request, Response};
use bohio_proto::info::Info;
use bohio_proto::{decode, encode, protocol};
use coqui_transport::Channel;

use crate::{Error, Open};

/// An open control stream. Calls are answered in order; every method takes `&self`, and calls
/// from several threads take turns.
pub struct Control {
    channel: Box<dyn Channel>,
    next_id: AtomicU64,
    turn: Mutex<()>,
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
            turn: Mutex::new(()),
        })
    }

    /// Sends `call` and waits up to `timeout` (`None`: without limit) for its answer.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host refused or failed the call, [`Error::Timeout`] when no
    /// answer came in time (the stream goes on; a late answer is skipped by the next call),
    /// [`Error::Closed`] when the host closed the stream, [`Error::Transport`] or
    /// [`Error::Protocol`] otherwise.
    pub fn call(&self, call: Call, timeout: Option<Duration>) -> Result<Reply, Error> {
        let _turn = self
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = encode(&Request { id, call })?;
        self.channel.send_timeout(&request, timeout)?;
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = self.channel.receive(left)? else {
                return Err(Error::Closed);
            };
            let response: Response = decode(&frame)?;
            // `0` answers a request the host could not read; any other id that is not ours is a
            // late answer to a call that timed out.
            if response.id == id || response.id == 0 {
                return response.into_result().map_err(Error::Remote);
            }
        }
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
