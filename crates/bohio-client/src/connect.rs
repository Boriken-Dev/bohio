//! A proven connection to a Bohío host: [`Connection`] is the [`Open`] every client call takes.
//!
//! The host proves itself to this side as this side proves itself to the host (a key each way),
//! with [`coqui_link`]'s proof; what follows is a link of streams, one per operation.

use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use coqui_link::auth::{Proof, Sealed, prove_out};
use coqui_link::mux::{LinkState, Mux, Side};
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure};

use crate::link::{Settings, socket};
use crate::relay::{Relay, relay};
use crate::{Error, Open};

/// One request to open a stream, answered on `reply`.
struct Request {
    protocol: String,
    metadata: Vec<u8>,
    reply: Sender<Result<Box<dyn Channel>, Error>>,
}

/// A proven connection to a host, owned: it ends when it is dropped. Streams are opened with the
/// [`Open`] trait, from any thread.
///
/// A thread runs the link; two more run for each open stream (see [`relay`](crate::relay)).
#[derive(Debug)]
pub struct Connection {
    requests: Mutex<Option<Sender<Request>>>,
    relays: Arc<Mutex<Vec<Weak<Relay>>>>,
    up: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
    host: PeerId,
}

/// What reaches the connection's own thread once the link is up.
struct Link {
    settings: Settings,
    requests: Receiver<Request>,
    relays: Arc<Mutex<Vec<Weak<Relay>>>>,
    up: Arc<AtomicBool>,
}

impl Connection {
    /// Connects to `address`, proves this side by `proof` and the host by the trust `proof` names,
    /// within `settings.proof_timeout` (and `timeout` for the socket).
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the host cannot be reached or ends the connection, [`Error::Link`]
    /// when the proof fails (a host whose key is not trusted ends the connection without a word, so
    /// that is a `Transport` failure too). A socket that connected to itself is refused.
    pub fn connect_tcp(
        address: SocketAddr,
        timeout: Duration,
        proof: &Proof,
        settings: &Settings,
    ) -> Result<Self, Error> {
        let stream = TcpStream::connect_timeout(&address, timeout)
            .map_err(|_| Error::Transport(Failure::PathLost))?;
        // A loopback connect to a closed port in the ephemeral range can connect a socket to itself.
        if stream.local_addr().ok() == stream.peer_addr().ok() {
            return Err(Error::Transport(Failure::PathLost));
        }
        let channel = socket(stream, settings).map_err(|_| Error::Transport(Failure::PathLost))?;
        Self::over(channel, proof, settings)
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
        let (requests, receiver) = mpsc::channel();
        let relays = Arc::new(Mutex::new(Vec::new()));
        let up = Arc::new(AtomicBool::new(false));
        let (ready, ready_rx) = mpsc::channel();
        let link = Link {
            settings: *settings,
            requests: receiver,
            relays: Arc::clone(&relays),
            up: Arc::clone(&up),
        };
        let proof = proof.clone();
        let thread = thread::Builder::new()
            .name("bohio-link".to_owned())
            .spawn(move || run(channel, &proof, link, &ready))
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
            requests: Mutex::new(Some(requests)),
            relays,
            up,
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
        self.up.load(Ordering::SeqCst)
    }

    /// Ends the connection: every stream is closed, and the link ends.
    pub fn close(&self) {
        drop(
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        for open in self
            .relays
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
        {
            open.close();
        }
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
        let (reply, answer) = mpsc::channel();
        {
            let requests = self.requests.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(requests) = requests.as_ref() else {
                return Err(Error::Closed);
            };
            requests
                .send(Request {
                    protocol: protocol.to_owned(),
                    metadata: metadata.to_vec(),
                    reply,
                })
                .map_err(|_| Error::Closed)?;
        }
        answer.recv().map_err(|_| Error::Closed)?
    }
}

/// What a client holds of a stream: closing or dropping it closes the relay.
struct Handle(Arc<Relay>);

impl Channel for Handle {
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

impl Drop for Handle {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The connection's own thread: proves, then serves open requests until the connection is dropped
/// or the link ends.
fn run<C: Channel>(channel: C, proof: &Proof, link: Link, ready: &Sender<Result<PeerId, Error>>) {
    let settings = link.settings;
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
        link.up.store(true, Ordering::SeqCst);
        let _ = ready.send(Ok(host));
        thread::scope(|scope| {
            loop {
                match link
                    .requests
                    .recv_timeout(settings.poll_interval.max(Duration::from_millis(1)))
                {
                    Ok(request) => {
                        let opened = mux
                            .open_with(&request.protocol, &request.metadata)
                            .map_err(Error::from);
                        let answer = opened.map(|stream| {
                            let relay = relay(scope, Arc::new(stream), &settings);
                            let mut relays =
                                link.relays.lock().unwrap_or_else(PoisonError::into_inner);
                            relays.retain(|known| known.strong_count() > 0);
                            relays.push(Arc::downgrade(&relay));
                            Box::new(Handle(relay)) as Box<dyn Channel>
                        });
                        let _ = request.reply.send(answer);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if !matches!(mux.link_state(), LinkState::Up) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            link.up.store(false, Ordering::SeqCst);
            // The connection is over: whatever is still open is closed, so the scope can end.
            for open in link
                .relays
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .filter_map(Weak::upgrade)
            {
                open.close();
            }
        });
    });
    channel.close();
}
