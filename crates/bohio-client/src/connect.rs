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
use coqui_link::link::{Capabilities, LINK_PROTOCOL, capabilities};
use coqui_link::mux::{
    CloseReason, LinkState, LinkStream, Mux, Side, Stream, StreamEnd, StreamLife,
};
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure};

use crate::dial::{Endpoint, ServerTrust};
use crate::exec::Exec;
use crate::forward::{Bytes, ForwardSettings, Target};
use crate::link::Settings;
use crate::relay::{Relay, relay_with};
use crate::{Error, Open, OpenOptions};

/// One request of the link's thread: open a stream, or read the peer's capability document.
enum Request {
    Open {
        protocol: String,
        metadata: Vec<u8>,
        options: OpenOptions,
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
    /// What a read asks for ([`Open::bulk_window`]).
    bulk_window: Option<u32>,
}

impl Shared {
    /// Tells every open stream its link ended: see [`Relay::link_ended`].
    fn link_ended(&self) {
        for open in self
            .relays
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
        {
            open.link_ended();
        }
    }

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
    ///
    /// `settings` say what a stream that carries a file to this side asks for: [`Open::bulk_window`].
    pub fn pair(settings: &Settings) -> (Self, Opens) {
        let (requests, receiver) = mpsc::channel();
        let shared = Arc::new(Shared {
            relays: Mutex::new(Vec::new()),
            ended: AtomicBool::new(false),
            bulk_window: (settings.bulk_window > settings.window).then_some(settings.bulk_window),
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
        self.open_stream_with(protocol, metadata, &OpenOptions::default())
    }

    /// As [`Opener::open_stream`], with `options`: a `receive_window` is granted on the new stream before it is returned, up to
    /// the link's `max_window`.
    ///
    /// # Errors
    ///
    /// As [`Opener::open_stream`].
    pub fn open_stream_with(
        &self,
        protocol: &str,
        metadata: &[u8],
        options: &OpenOptions,
    ) -> Result<Opened, Error> {
        if !self.is_up() {
            return Err(Error::Closed);
        }
        let (reply, answer) = mpsc::channel();
        self.requests
            .send(Request::Open {
                protocol: protocol.to_owned(),
                metadata: metadata.to_vec(),
                options: *options,
                reply,
            })
            .map_err(|_| Error::Closed)?;
        answer.recv().map_err(|_| Error::Closed)?
    }
}

impl Open for Opener {
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        self.open_stream(protocol, metadata)
            .map(|opened| Box::new(opened) as Box<dyn LinkStream>)
    }

    fn open_cause(
        &self,
        protocol: &str,
        metadata: &[u8],
    ) -> Result<(Box<dyn LinkStream>, Cause), Error> {
        self.open_stream(protocol, metadata).map(|opened| {
            let cause = opened.cause();
            (Box::new(opened) as Box<dyn LinkStream>, cause)
        })
    }

    fn open_with(
        &self,
        protocol: &str,
        metadata: &[u8],
        options: &OpenOptions,
    ) -> Result<Box<dyn LinkStream>, Error> {
        self.open_stream_with(protocol, metadata, options)
            .map(|opened| Box::new(opened) as Box<dyn LinkStream>)
    }

    fn bulk_window(&self) -> Option<u32> {
        self.shared.bulk_window
    }

    fn is_up(&self) -> bool {
        Self::is_up(self)
    }
}

impl Opens {
    /// Serves open requests on `mux` until the link leaves `Up`, [`Opener::close`] is called or every
    /// [`Opener`] is gone. Each stream is handed back through a bounded [`relay`](crate::relay::relay), two threads that live
    /// as long as the stream does; whatever is still open when this returns is closed.
    pub fn run<C: Channel>(self, mux: &Mux<'_, C>, settings: &Settings) {
        let poll = settings.poll_interval.max(Duration::from_millis(1));
        thread::scope(|scope| {
            // The peer's `link/1` says its own window and frame: a difference from this side's is worth one line. The
            // stream is read between requests and dropped with the loop, so a link that ends first waits for no answer.
            let mut probe = mux.open(LINK_PROTOCOL).ok();
            loop {
                if self.shared.ended.load(Ordering::SeqCst) {
                    break;
                }
                if let Some(stream) = &probe {
                    match stream.receive(Some(Duration::ZERO)) {
                        Err(Failure::Timeout) => {}
                        Ok(Some(document)) => {
                            report_difference(settings, &document);
                            probe = None;
                        }
                        Ok(None) | Err(_) => probe = None,
                    }
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
                        options,
                        reply,
                    }) => {
                        let opened = mux.open_with(&protocol, &metadata).map_err(Error::from);
                        let answer = opened.map(|stream| {
                            if let Some(wanted) = options.receive_window {
                                widen(&stream, wanted);
                            }
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
            drop(probe);
            self.shared.ended.store(true, Ordering::SeqCst);
            // The link is over. What each stream received and how it ended still reaches its reader, for at most `send_limit`
            // each, so a transfer cut by a revoke reads as one and the scope can still end.
            self.shared.link_ended();
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
    /// The link this one runs over when it was made [`through`](Connection::through) a host: kept up as long as this one is.
    carrier: Option<Carrier>,
}

/// The link under a jumped connection.
struct Carrier {
    _under: Arc<dyn Open>,
}

impl core::fmt::Debug for Carrier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Carrier")
    }
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
        let (opener, opens) = Opener::pair(settings);
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
            carrier: None,
        })
    }

    /// Connects to `endpoint` *through* the host `link` reaches, as `ssh -J` does: the host carries a stream to the endpoint
    /// (a TCP address, a Unix socket or pipe on that host's machine, or the standard streams of a program it runs for this
    /// peer) and this side proves itself, and the host at the far end, over it with `proof`. The jump host sees the link's
    /// sealed bytes and nothing else, so whichever host answers must be the one `proof` trusts: a jump cannot stand in.
    ///
    /// What the previous host must grant this peer: `forward.connect` and a rule that covers the endpoint's target for a
    /// `tcp://` or `pipe://` endpoint, or `exec` of the program (with its standard input open) for a `stdio://` one. The
    /// connection keeps `link` alive and is closed before it. A `pipe://` endpoint names a socket or pipe in the text the
    /// local system parses it with; a Windows pipe is `pipe://\.\pipe\NAME` from a Windows client.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] or [`Error::Transport`] with `Permission` when the previous host refuses the stream (the
    /// grant does not cover the target, or the program is not allowed); [`Error::Link`] for an endpoint kind a jump does not
    /// carry (`ws://`, `wss://`, `webtransport://`, `serial://`) or when the proof fails; otherwise as [`Connection::connect`].
    pub fn through(
        link: Arc<dyn Open>,
        endpoint: &Endpoint,
        proof: &Proof,
        settings: &Settings,
        forward: &ForwardSettings,
    ) -> Result<Self, Error> {
        let bytes = match endpoint {
            Endpoint::Tcp(address) => Bytes::tunnel(
                crate::forward::connect(
                    &*link,
                    &Target::Tcp {
                        host: address.ip().to_string(),
                        port: address.port(),
                    },
                    forward,
                )?,
                forward,
            ),
            Endpoint::Pipe(name) => {
                let path = name.path().to_string_lossy().into_owned();
                let target = match path.strip_prefix(r"\.\pipe\") {
                    Some(pipe) => Target::Pipe {
                        name: pipe.to_owned(),
                    },
                    None => Target::Unix { path },
                };
                Bytes::tunnel(crate::forward::connect(&*link, &target, forward)?, forward)
            }
            Endpoint::Command(argv) => {
                let mut request = bohio_proto::exec::Request::argv(argv.iter().cloned());
                request.stdin = bohio_proto::exec::Stdin::Open;
                let exec = Exec::start(&*link, &request)?;
                // The host's refusal (no grant, a program that is not allowed or not there) is the answer, not a link that
                // never proves: wait for the program to start.
                loop {
                    match exec.next(Some(forward.answer_timeout))? {
                        Some(crate::exec::Event::Started { .. }) => break,
                        Some(crate::exec::Event::Exit(exit)) => {
                            return Err(Error::Link(format!(
                                "the program ended before the link began (status {:?})",
                                exit.code
                            )));
                        }
                        Some(_) => {}
                        None => return Err(Error::Closed),
                    }
                }
                Bytes::program(exec, forward)
            }
            other => {
                return Err(Error::Link(format!(
                    "a jump cannot carry {other}: only tcp://, pipe:// and stdio:// endpoints go through a host"
                )));
            }
        };
        let channel = crate::link::stream(bytes, settings).map_err(|error| {
            Error::Link(format!("the stream cannot be framed: {:?}", error.kind()))
        })?;
        let mut connection = Self::over(channel, proof, settings)?;
        connection.carrier = Some(Carrier { _under: link });
        Ok(connection)
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
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        self.opener.open(protocol, metadata)
    }

    fn open_cause(
        &self,
        protocol: &str,
        metadata: &[u8],
    ) -> Result<(Box<dyn LinkStream>, Cause), Error> {
        self.opener.open_cause(protocol, metadata)
    }

    fn open_with(
        &self,
        protocol: &str,
        metadata: &[u8],
        options: &OpenOptions,
    ) -> Result<Box<dyn LinkStream>, Error> {
        self.opener.open_with(protocol, metadata, options)
    }

    fn bulk_window(&self) -> Option<u32> {
        self.opener.bulk_window()
    }

    fn is_up(&self) -> bool {
        Self::is_up(self)
    }
}

/// Grants what `stream`'s receive window lacks of `wanted`; the link caps it at its `max_window`.
fn widen<C: Channel>(stream: &Stream<'_, C>, wanted: u32) {
    let have = stream.window();
    if wanted <= have {
        return;
    }
    // A stream that ended before it was widened tells its owner so itself.
    if let Ok(now) = stream.grant(wanted - have)
        && now < wanted
    {
        tracing::debug!(wanted, now, "a stream's window was capped at max_window");
    }
}

/// Logs, as an event and not a refusal, that the peer's initial window or frame differs from this side's.
///
/// The initial window is not negotiated: each sender starts a stream with its own as credit, and a receiver
/// ends a stream whose sender is ahead of its own. A peer with the larger window can therefore break the other's
/// streams, and one with the larger frame is refused; the safe way to widen one stream is `bulk_window`.
fn report_difference(settings: &Settings, document: &[u8]) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(document) else {
        return;
    };
    let window = value
        .get("window")
        .and_then(serde_json::Value::as_u64)
        .and_then(|window| u32::try_from(window).ok());
    let max_frame = value
        .get("max_frame")
        .and_then(serde_json::Value::as_u64)
        .and_then(|frame| usize::try_from(frame).ok());
    if let Some(window) = window.filter(|window| *window != settings.window) {
        if window > settings.window {
            tracing::warn!(
                peer = window,
                ours = settings.window,
                "the peer's [link] window is larger than ours: a stream it fills before this side grants more ends as broken; \
                 keep `window` equal on both sides and widen a bulk stream with `bulk_window`"
            );
        } else {
            tracing::warn!(
                peer = window,
                ours = settings.window,
                "the peer's [link] window is smaller than ours: its streams carry that much until this side grants more"
            );
        }
    }
    if let Some(frame) = max_frame.filter(|frame| *frame != settings.max_frame) {
        tracing::warn!(
            peer = frame,
            ours = settings.max_frame,
            "the peer's [link] max_frame differs from ours: a frame larger than the receiver's is refused, so keep it equal"
        );
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

    /// A handle that says the same after the stream itself has been boxed as a `dyn LinkStream`.
    pub fn cause(&self) -> Cause {
        Cause(Some(Arc::clone(&self.0)))
    }
}

/// The text a link's host gives when it refuses an open because a limit is full (`[link] max_pending_opens`, `max_streams`,
/// `max_per_protocol` or `max_opens_per_minute`), as `coqui_link` writes it. A refusal for a grant says something else.
pub const OVER_A_LIMIT: &str = "over a limit";

/// Why a stream ended, kept apart from the stream (which a caller holds as a `dyn LinkStream`): see [`Open::open_cause`].
#[derive(Debug, Clone, Default)]
pub struct Cause(Option<Arc<Relay>>);

impl Cause {
    /// A cause that never says anything: for a link that cannot tell.
    #[must_use]
    pub const fn unknown() -> Self {
        Self(None)
    }

    /// Why the stream ended, with the text its peer gave, once it has.
    #[must_use]
    pub fn end(&self) -> Option<StreamEnd> {
        self.0.as_ref().and_then(|relay| relay.end())
    }

    /// Whether the host refused the stream because a limit was full ([`OVER_A_LIMIT`]): the same open may succeed a moment
    /// later.
    #[must_use]
    pub fn over_a_limit(&self) -> bool {
        matches!(self.end(), Some(StreamEnd::Refused(text)) if text == OVER_A_LIMIT)
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

impl LinkStream for Opened {
    fn life(&self) -> StreamLife {
        self.0.life()
    }

    fn finish(&self) -> Result<(), Failure> {
        self.0.finish()
    }

    fn finished_by_peer(&self) -> bool {
        self.0.finished_by_peer()
    }

    fn close_with(&self, reason: CloseReason, text: &str) {
        self.0.close_with(reason, text);
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
