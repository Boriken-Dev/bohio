//! A proven connection to a Bohío host: [`Connection`] is the [`Open`] every client call takes.
//!
//! The host proves itself to this side as this side proves itself to the host (a key each way),
//! with [`coqui_link`]'s proof; what follows is a link of streams, one per operation.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use coqui_link::auth::{Proof, Sealed, prove_out};
use coqui_link::link::{Capabilities, capabilities};
use coqui_link::mux::{LinkState, Mux, Side, Stream, StreamEnd};
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure};

use crate::dial::{Endpoint, ServerTrust};
use crate::link::Settings;
use crate::relay::{Relay, relay_with};
use crate::{Error, Open};

/// One request of the link's thread: open a stream, or read the peer's capability document.
enum Request {
    Open {
        protocol: String,
        metadata: Vec<u8>,
        reply: Sender<Result<Opened, Error>>,
    },
    Capabilities {
        wait: Duration,
        reply: Sender<Result<Capabilities, Error>>,
    },
}

/// What an [`Opener`] and its [`Opens`] share.
#[derive(Debug)]
struct Shared {
    relays: Mutex<Vec<Weak<Relay>>>,
    /// Set by [`Opener::close`], or when the link ends.
    ended: AtomicBool,
}

impl Shared {
    fn close_relays(&self) {
        for open in self
            .relays
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
        {
            open.close();
        }
    }
}

/// Opens streams on a link whose [`Opens`] is being run: an [`Open`] that is cheap to clone and hand to
/// other threads. It is made with [`Opener::pair`].
///
/// An open waits for the link's thread to make the stream, so it fails with [`Error::Closed`] once the
/// link has ended or the opener was closed.
#[derive(Debug, Clone)]
pub struct Opener {
    requests: Sender<Request>,
    shared: Arc<Shared>,
}

/// The serving half of an [`Opener`]: [`Opens::run`] answers the open requests on a link.
#[derive(Debug)]
pub struct Opens {
    requests: Receiver<Request>,
    shared: Arc<Shared>,
}

impl Opener {
    /// A new opener and the [`Opens`] that serves it. Nothing opens until `Opens::run` runs on a link.
    pub fn pair() -> (Self, Opens) {
        let (requests, receiver) = mpsc::channel();
        let shared = Arc::new(Shared {
            relays: Mutex::new(Vec::new()),
            ended: AtomicBool::new(false),
        });
        (
            Self {
                requests,
                shared: Arc::clone(&shared),
            },
            Opens {
                requests: receiver,
                shared,
            },
        )
    }

    /// Whether the link can still open streams: `false` once it ended or [`Opener::close`] was called.
    pub fn is_up(&self) -> bool {
        !self.shared.ended.load(Ordering::SeqCst)
    }

    /// Closes every stream this opener opened and makes [`Opens::run`] return, which ends a link made
    /// only to serve it.
    pub fn close(&self) {
        self.shared.ended.store(true, Ordering::SeqCst);
        self.shared.close_relays();
    }

    /// Reads the peer's `link/1` document now: what it offers this side at this moment. The link's
    /// thread opens the stream and waits up to `wait` for the document.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] when the link has ended, [`Error::Transport`] when the document does not come
    /// within `wait` or is not one.
    pub fn capabilities(&self, wait: Duration) -> Result<Capabilities, Error> {
        if !self.is_up() {
            return Err(Error::Closed);
        }
        let (reply, answer) = mpsc::channel();
        self.requests
            .send(Request::Capabilities { wait, reply })
            .map_err(|_| Error::Closed)?;
        answer.recv().map_err(|_| Error::Closed)?
    }
}

impl Opener {
    /// Opens a stream for `protocol` carrying `metadata`, as [`Open::open`] does, keeping the type that can
    /// say why the stream ended.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] when the link has ended, else whatever the open reports.
    pub fn open_stream(&self, protocol: &str, metadata: &[u8]) -> Result<Opened, Error> {
        if !self.is_up() {
            return Err(Error::Closed);
        }
        let (reply, answer) = mpsc::channel();
        self.requests
            .send(Request::Open {
                protocol: protocol.to_owned(),
                metadata: metadata.to_vec(),
                reply,
            })
            .map_err(|_| Error::Closed)?;
        answer.recv().map_err(|_| Error::Closed)?
    }
}

impl Open for Opener {
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn Channel>, Error> {
        self.open_stream(protocol, metadata)
            .map(|opened| Box::new(opened) as Box<dyn Channel>)
    }
}

impl Opens {
    /// Serves open requests on `mux` until the link leaves `Up`, [`Opener::close`] is called or every
    /// [`Opener`] is gone. Each stream is handed back through a bounded [`relay`], two threads that live
    /// as long as the stream does; whatever is still open when this returns is closed.
    pub fn run<C: Channel>(self, mux: &Mux<'_, C>, settings: &Settings) {
        let poll = settings.poll_interval.max(Duration::from_millis(1));
        thread::scope(|scope| {
            loop {
                if self.shared.ended.load(Ordering::SeqCst) {
                    break;
                }
                match self.requests.recv_timeout(poll) {
                    Ok(Request::Capabilities { wait, reply }) => {
                        // A slow peer holds up no open: the read has a thread of its own.
                        scope.spawn(move || {
                            let _ = reply.send(capabilities(mux, wait).map_err(Error::from));
                        });
                    }
                    Ok(Request::Open {
                        protocol,
                        metadata,
                        reply,
                    }) => {
                        let opened = mux.open_with(&protocol, &metadata).map_err(Error::from);
                        let answer = opened.map(|stream| {
                            let relay = relay_with(
                                scope,
                                Arc::new(stream),
                                settings,
                                |stream: &Stream<'_, C>| stream.end(),
                            );
                            let mut relays = self
                                .shared
                                .relays
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            relays.retain(|known| known.strong_count() > 0);
                            relays.push(Arc::downgrade(&relay));
                            Opened(relay)
                        });
                        let _ = reply.send(answer);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if !matches!(mux.link_state(), LinkState::Up) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            self.shared.ended.store(true, Ordering::SeqCst);
            // The link is over: whatever is still open is closed, so the scope can end.
            self.shared.close_relays();
        });
    }
}

/// A proven connection to a host, owned: it ends when it is dropped. Streams are opened with the
/// [`Open`] trait, from any thread.
///
/// A thread runs the link; two more run for each open stream (see [`relay`](crate::relay)).
#[derive(Debug)]
pub struct Connection {
    opener: Opener,
    thread: Mutex<Option<JoinHandle<()>>>,
    host: PeerId,
}

impl Connection {
    /// Connects to `endpoint`, proves this side by `proof` and the host by the trust `proof` names,
    /// within `settings.proof_timeout` (and `timeout` for opening the path: the socket, the pipe, the
    /// WebSocket handshake, the port or the program).
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the host cannot be reached or ends the connection, [`Error::Link`]
    /// when the proof fails (a host whose key is not trusted ends the connection without a word, so
    /// that is a `Transport` failure too). A TCP socket that connected to itself is refused.
    pub fn connect(
        endpoint: &Endpoint,
        timeout: Duration,
        proof: &Proof,
        settings: &Settings,
    ) -> Result<Self, Error> {
        Self::connect_with(endpoint, timeout, proof, settings, &ServerTrust::default())
    }

    /// As [`Connection::connect`], checking a TLS host's certificate by `trust`.
    ///
    /// # Errors
    ///
    /// As [`Connection::connect`]; a certificate `trust` refuses is `Error::Transport(Failure::Certificate)`.
    pub fn connect_with(
        endpoint: &Endpoint,
        timeout: Duration,
        proof: &Proof,
        settings: &Settings,
        trust: &ServerTrust,
    ) -> Result<Self, Error> {
        let channel = endpoint.dial_with(timeout, settings, trust)?;
        Self::over(channel, proof, settings)
    }

    /// Connects to the TCP `address`: [`Connection::connect`] for a `tcp://` endpoint.
    ///
    /// # Errors
    ///
    /// As [`Connection::connect`].
    pub fn connect_tcp(
        address: SocketAddr,
        timeout: Duration,
        proof: &Proof,
        settings: &Settings,
    ) -> Result<Self, Error> {
        Self::connect(&Endpoint::Tcp(address), timeout, proof, settings)
    }

    /// Proves over `channel`, an unproven channel to the host, and starts the link.
    ///
    /// # Errors
    ///
    /// As [`Connection::connect_tcp`].
    pub fn over<C: Channel + 'static>(
        channel: C,
        proof: &Proof,
        settings: &Settings,
    ) -> Result<Self, Error> {
        let (opener, opens) = Opener::pair();
        let (ready, ready_rx) = mpsc::channel();
        let (settings, proof) = (*settings, proof.clone());
        let thread = thread::Builder::new()
            .name("bohio-link".to_owned())
            .spawn(move || run(channel, &proof, opens, &settings, &ready))
            .map_err(|_| Error::Transport(Failure::Cancelled))?;
        let host = match ready_rx.recv() {
            Ok(Ok(host)) => host,
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(Error::Closed);
            }
        };
        Ok(Self {
            opener,
            thread: Mutex::new(Some(thread)),
            host,
        })
    }

    /// The host's proven id.
    pub const fn host(&self) -> PeerId {
        self.host
    }

    /// Whether the link is up: `false` once the host or the network ended it.
    pub fn is_up(&self) -> bool {
        self.opener.is_up()
    }

    /// An [`Opener`] for this connection's link, to hand to other threads. It does not keep the
    /// connection alive: the link ends when the connection is closed or dropped.
    pub fn opener(&self) -> Opener {
        self.opener.clone()
    }

    /// Ends the connection: every stream is closed, and the link ends.
    pub fn close(&self) {
        self.opener.close();
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

impl Open for Connection {
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn Channel>, Error> {
        self.opener.open(protocol, metadata)
    }
}

/// What a client holds of a stream: closing or dropping it closes the relay. [`Opened::end`] says why
/// the stream ended, with the text its peer gave.
#[derive(Debug)]
pub struct Opened(Arc<Relay>);

impl Opened {
    /// Why the stream ended, with the text its peer gave (a refusal, a revoke), once it has.
    pub fn end(&self) -> Option<StreamEnd> {
        self.0.end()
    }
}

impl Channel for Opened {
    fn send(&self, message: &[u8]) -> Result<(), Failure> {
        self.0.send(message)
    }

    fn send_timeout(&self, message: &[u8], timeout: Option<Duration>) -> Result<(), Failure> {
        self.0.send_timeout(message, timeout)
    }

    fn receive(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, Failure> {
        self.0.receive(timeout)
    }

    fn close(&self) {
        self.0.close();
    }

    fn capabilities(&self) -> coqui_transport::Capabilities {
        self.0.capabilities()
    }

    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The connection's own thread: proves, then serves open requests until the connection is dropped
/// or the link ends.
fn run<C: Channel>(
    channel: C,
    proof: &Proof,
    opens: Opens,
    settings: &Settings,
    ready: &Sender<Result<PeerId, Error>>,
) {
    let proved = prove_out(
        proof,
        &channel,
        settings.proof_timeout,
        settings.record_length(proof.method()),
    );
    let established = match proved {
        Ok(established) => established,
        Err(error) => {
            channel.close();
            let _ = ready.send(Err(match error {
                coqui_link::Error::Channel(failure) => Error::Transport(failure),
                other => Error::Link(other.to_string()),
            }));
            return;
        }
    };
    let _ = channel.set_pending(settings.read_ahead.max(1));
    let host = established.peer;
    let sealed = Sealed::new(&channel, established.session);
    Mux::run(&sealed, Side::Dialed, settings.limits(), None, |mux| {
        let _ = ready.send(Ok(host));
        opens.run(mux, settings);
    });
    channel.close();
}
