//! Forwarding: `bohio-connect/1`, a connection from the host to a target, as a byte stream here.
//!
//! [`connect`] opens the stream, waits for the host's answer (a refusal comes back as [`Error::Remote`] with the reason, never as
//! a stream that silently ends) and gives a [`Tunnel`]: a stream of raw bytes with half-close. A tunnel is handed to one of
//! the pumps ([`Tunnel::pump_tcp`], [`Tunnel::pump_ipc`]) or bridged to standard streams ([`Tunnel::bridge`]), and
//! [`listen_tcp`] and [`listen_ipc`] make one per connection a local listener accepts (`ssh -L`).

mod bytes;
pub mod ipc;
mod remote;

pub use bytes::{Bytes, BytesReader, BytesWriter};

pub use remote::{
    Arrival, Bind, ListenRequest, LocalConnection, RemoteForward, RemoteListener, connect_local,
    listen,
};

use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use bohio_proto::forward::Answer;
pub use bohio_proto::forward::{ConnectRequest, Connected, Target};
pub use coqui_link::forward::Options;
use coqui_link::mux::LinkStream;
use coqui_transport::Failure;

use crate::keep::Jitter;
use crate::{Error, Open};

/// What a forward waits for and how its pump moves bytes. Every value has a default; a zero in [`ForwardSettings::options`]
/// is refused by [`Options::validate`] when a pump starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ForwardSettings {
    /// The pump's bounds: chunk size, how long a stalled write waits, the intervals of its polling and the listener's.
    pub options: Options,
    /// How long to wait for the host's answer to a connect: it covers the host's own connect timeout and the link's round
    /// trip. Default 30 seconds.
    pub answer_timeout: Duration,
    /// The most connections a local listener serves at once; one past it is closed at once. At least 1. Default 64.
    pub max_connections: usize,
    /// How long an open the host refused because it was over a limit (`[link] max_pending_opens` and the like) is tried
    /// again before the connection fails. Zero never retries. Default 10 seconds.
    pub open_retry: Duration,
    /// How long to wait between those attempts, each moved by up to half of itself. Not zero. Default 100 milliseconds.
    pub open_retry_interval: Duration,
    /// How long a connect waits for a link to be up, and for the host's answer on a link that ended before it came, when the
    /// link is kept ([`keep::Relink`](crate::keep::Relink)). Default 30 seconds.
    pub wait_for_link: Duration,
    /// How long a SOCKS5 client ([`socks::serve`](crate::socks::serve)) has to finish its handshake: its greeting and its request.
    /// A client that is slower is closed. Not zero. Default 10 seconds.
    pub socks_timeout: Duration,
}

impl Default for ForwardSettings {
    fn default() -> Self {
        Self {
            options: Options::default(),
            answer_timeout: Duration::from_secs(30),
            max_connections: 64,
            open_retry: Duration::from_secs(10),
            open_retry_interval: Duration::from_millis(100),
            wait_for_link: Duration::from_secs(30),
            socks_timeout: Duration::from_secs(10),
        }
    }
}

/// A connected forward: the host's connection to the target, as a stream of raw bytes in both directions. Dropping it closes
/// the stream, which resets the host's connection unless both directions ended in order.
pub struct Tunnel {
    stream: Box<dyn LinkStream>,
    address: Option<String>,
    options: Options,
}

impl core::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tunnel")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

/// Connects the host to `target` and waits for its answer.
///
/// # Errors
///
/// [`Error::Remote`] when the host did not connect (`refused`, `unreachable`, `timeout`, `not_found`, `busy`, `unsupported`,
/// `bad_request`); [`Error::Transport`] with `Permission` when the peer's grant does not cover the target or was revoked,
/// before anything was connected; [`Error::Timeout`] when no answer came within [`ForwardSettings::answer_timeout`];
/// [`Error::Closed`] when the stream ended with no answer.
pub fn connect(
    link: &dyn Open,
    target: &Target,
    settings: &ForwardSettings,
) -> Result<Tunnel, Error> {
    let request = ConnectRequest::new(target.clone());
    open_answered(
        link,
        bohio_proto::protocol::CONNECT,
        &request.to_metadata()?,
        settings,
    )
}

/// Opens `protocol` with `metadata` and waits for the host's one answer, `connected` or an error: what `bohio-connect/1` and
/// `bohio-accept/1` both begin with.
fn open_answered(
    link: &dyn Open,
    protocol: &str,
    metadata: &[u8],
    settings: &ForwardSettings,
) -> Result<Tunnel, Error> {
    let began = Instant::now();
    let mut jitter = Jitter::new();
    loop {
        let (stream, frame) = open_first(link, protocol, metadata, settings)?;
        match Answer::decode(&frame) {
            Ok(Answer::Connected(connected)) => {
                return Ok(Tunnel {
                    stream,
                    address: connected.address,
                    options: settings.options,
                });
            }
            // The host's own bounds on forwarded connections are full: a burst is served as it drains.
            Ok(Answer::Error(body))
                if body.code == bohio_proto::ErrorCode::Busy
                    && began.elapsed() < settings.open_retry =>
            {
                stream.close();
                thread::sleep(retry_pause(settings, &mut jitter));
            }
            Ok(Answer::Error(body)) => {
                stream.close();
                return Err(Error::Remote(body));
            }
            Ok(_) | Err(_) => {
                stream.close();
                return Err(Error::Protocol(bohio_proto::Error::Decode));
            }
        }
    }
}

/// How long to wait before an open is tried again: `open_retry_interval`, moved by up to a quarter of itself.
fn retry_pause(settings: &ForwardSettings, jitter: &mut Jitter) -> Duration {
    settings
        .open_retry_interval
        .mul_f64(1.0 + 0.25 * jitter.sample())
}

/// Opens `protocol` with `metadata` and reads the host's first frame, trying again where trying again can help: the host
/// refused the open because it was over a limit (for [`ForwardSettings::open_retry`]), or a kept link ended before the
/// host answered (for [`ForwardSettings::wait_for_link`]). Anything else is the answer.
pub(crate) fn open_first(
    link: &dyn Open,
    protocol: &str,
    metadata: &[u8],
    settings: &ForwardSettings,
) -> Result<(Box<dyn LinkStream>, Vec<u8>), Error> {
    let began = Instant::now();
    let mut jitter = Jitter::new();
    loop {
        let (stream, cause) = match link.open_cause(protocol, metadata) {
            Ok(opened) => opened,
            // The link's queue was full when the open was sent: the same "try again" as below.
            Err(Error::Transport(Failure::NoProgress)) if began.elapsed() < settings.open_retry => {
                thread::sleep(retry_pause(settings, &mut jitter));
                continue;
            }
            Err(error) => return Err(error),
        };
        let failure = match stream.receive(Some(settings.answer_timeout)) {
            Ok(Some(frame)) => return Ok((stream, frame)),
            Ok(None) => {
                stream.close();
                return Err(Error::Closed);
            }
            Err(failure) => failure,
        };
        stream.close();
        // `NoProgress` is the link's own "full, try again": the open could not be queued behind the others of a burst.
        let pause = if (failure == Failure::Permission && cause.over_a_limit())
            || failure == Failure::NoProgress
        {
            (began.elapsed() < settings.open_retry).then(|| retry_pause(settings, &mut jitter))
        } else if failure == Failure::PathLost && link.relink_wait().is_some() {
            (began.elapsed() < settings.wait_for_link).then_some(settings.options.listen_poll)
        } else {
            None
        };
        match pause {
            Some(pause) => thread::sleep(pause),
            None => return Err(Error::from(failure)),
        }
    }
}

impl Tunnel {
    /// The address the host's connection reached, when the host said (a TCP target's resolved address).
    pub fn address(&self) -> Option<&str> {
        self.address.as_deref()
    }

    /// The byte stream: raw messages both ways, `finish` as half-close.
    pub fn stream(&self) -> &dyn LinkStream {
        &*self.stream
    }

    /// The byte stream, owned.
    pub fn into_stream(self) -> Box<dyn LinkStream> {
        self.stream
    }

    /// Moves bytes between this tunnel and `connection` until both directions end, then closes the tunnel: see
    /// [`coqui_link::forward::pump_with`].
    pub fn pump_tcp(self, connection: &std::net::TcpStream) {
        coqui_link::forward::pump_with(&*self.stream, connection, &self.options);
        self.stream.close();
    }

    /// As [`Tunnel::pump_tcp`], for a local Unix socket or pipe connection: see [`ipc::pump`].
    pub fn pump_ipc(self, connection: coqui_transport_pipe::Stream) {
        ipc::pump(&*self.stream, connection, &self.options);
        self.stream.close();
    }

    /// Moves `input` to the host and the host's bytes to `output`, until the host ends its direction, then closes the tunnel:
    /// netcat on a tunnel. The end of `input` is a half-close, and the reply that follows is still read.
    ///
    /// `input` is read on a thread of its own that outlives this call when `input` is blocked (a terminal that nobody types
    /// at), so give it something the process can abandon, such as standard input.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] when the host ended the stream without a `finish` (the target's end did not arrive in order),
    /// [`Error::Transport`] when the stream failed or the host revoked it, and [`Error::Link`] when `output` fails.
    pub fn bridge(
        self,
        mut input: impl Read + Send + 'static,
        output: &mut dyn Write,
    ) -> Result<(), Error> {
        let (stream, options) = (self.stream, self.options);
        let stream: Arc<dyn LinkStream> = Arc::from(stream);
        let sender = Arc::clone(&stream);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        thread::spawn(move || {
            let mut buffer = vec![0_u8; options.chunk.max(1)];
            loop {
                match input.read(&mut buffer) {
                    Ok(0) => {
                        let _ = sender.finish();
                        return;
                    }
                    Ok(read) => {
                        if !ipc::send_all(&*sender, &buffer[..read], &options, &stopped) {
                            return;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        let _ = sender.finish();
                        return;
                    }
                }
            }
        });
        let result = loop {
            match stream.receive(Some(options.watch)) {
                Ok(Some(message)) => {
                    if let Err(error) = output.write_all(&message).and_then(|()| output.flush()) {
                        break Err(Error::Link(format!("the output failed: {}", error.kind())));
                    }
                }
                Ok(None) if stream.finished_by_peer() => break Ok(()),
                Ok(None) => break Err(Error::Closed),
                Err(Failure::Timeout) => {}
                Err(failure) => break Err(Error::Transport(failure)),
            }
        };
        stop.store(true, Ordering::SeqCst);
        stream.close();
        result
    }
}

/// Why a local listener stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ended {
    /// The caller's stop flag was set.
    Stopped,
    /// The link to the host ended.
    LinkLost,
    /// The host ended a remote listener while the link stayed up: its grant no longer covers the bind, or the listener
    /// failed (the reason went to the forward's `report`).
    Closed,
}

/// Serves `listener` until `stop` is set or the link ends: each connection is a [`connect`] to `target` and a
/// [`Tunnel::pump_tcp`]. A connection whose connect fails is closed, and the reason goes to `report`. Whoever can connect to `listener` has what this peer is granted, so bind it to
/// loopback.
///
/// # Errors
///
/// The listener's error.
pub fn listen_tcp(
    link: &dyn Open,
    listener: &TcpListener,
    target: &Target,
    settings: &ForwardSettings,
    stop: &AtomicBool,
    report: &(dyn Fn(&Error) + Sync),
) -> io::Result<Ended> {
    settings
        .options
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    listener.set_nonblocking(true)?;
    let live = AtomicUsize::new(0);
    thread::scope(|scope| {
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(Ended::Stopped);
            }
            if !link.is_up() {
                return Ok(Ended::LinkLost);
            }
            match listener.accept() {
                Ok((connection, _)) => {
                    if live.load(Ordering::SeqCst) >= settings.max_connections.max(1) {
                        continue;
                    }
                    live.fetch_add(1, Ordering::SeqCst);
                    let live = &live;
                    scope.spawn(move || {
                        match connect(link, target, settings) {
                            Ok(tunnel) => tunnel.pump_tcp(&connection),
                            Err(error) => report(&error),
                        }
                        live.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(settings.options.listen_poll);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // A connection that was reset before it was accepted is not the listener's failure.
                Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => {}
                Err(error) => return Err(error),
            }
        }
    })
}

/// As [`listen_tcp`], for a local Unix socket or pipe made with [`ipc::listener`].
///
/// # Errors
///
/// The listener's error.
pub fn listen_ipc(
    link: &dyn Open,
    listener: &coqui_transport_pipe::Listener,
    target: &Target,
    settings: &ForwardSettings,
    stop: &AtomicBool,
    report: &(dyn Fn(&Error) + Sync),
) -> io::Result<Ended> {
    settings
        .options
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let live = AtomicUsize::new(0);
    thread::scope(|scope| {
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(Ended::Stopped);
            }
            if !link.is_up() {
                return Ok(Ended::LinkLost);
            }
            match listener.accept_timeout(Some(settings.options.listen_poll)) {
                Ok(connection) => {
                    if live.load(Ordering::SeqCst) >= settings.max_connections.max(1) {
                        continue;
                    }
                    live.fetch_add(1, Ordering::SeqCst);
                    let live = &live;
                    scope.spawn(move || {
                        match connect(link, target, settings) {
                            Ok(tunnel) => tunnel.pump_ipc(connection),
                            Err(error) => report(&error),
                        }
                        live.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(coqui_transport_pipe::Error::TimedOut) => {}
                Err(error) => return Err(error.into()),
            }
        }
    })
}
