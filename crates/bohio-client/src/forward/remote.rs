//! Remote forwards: `bohio-listen/1` and `bohio-accept/1`, the host listening for this side.
//!
//! [`listen`] asks the host to bind and gives a [`RemoteListener`]: its address and the stream that announces each connection
//! the host takes. [`RemoteListener::accept`] claims one announced connection and gives a [`Tunnel`] of its bytes, which
//! is handed to a pump or bridged to standard streams like a connect's. [`RemoteForward`] is `ssh -R`: it does that for every
//! arrival on a thread of its own and connects each to a local target.
//!
//! Dropping a listener closes its stream, which ends the host's listener and resets the connections it handed over.

use std::io;
use std::net::{TcpStream, ToSocketAddrs as _};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bohio_proto::forward::{AcceptRequest, ListenFrame, Target};
pub use bohio_proto::forward::{Arrival, Bind, ListenRequest};
use cabuya::mux::LinkStream;
use coqui_transport::Failure;
use coqui_transport_pipe::PipeName;

use super::{Ended, ForwardSettings, Tunnel, ipc, open_answered, open_first};
use crate::{Error, Open};

/// A listener the host bound for this peer.
pub struct RemoteListener {
    stream: Box<dyn LinkStream>,
    address: String,
    token: String,
}

impl core::fmt::Debug for RemoteListener {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RemoteListener")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

/// Asks the host to bind `bind` and waits for its answer. `backlog` lowers how many arrivals the host holds waiting for
/// this listener below its own `[forward] max_pending`.
///
/// # Errors
///
/// [`Error::Remote`] when the host did not bind (`in_use`, `busy`, `unsupported`, `denied`, `bad_request`, `unreachable`,
/// `not_found`); [`Error::Transport`] with `Permission` when the peer's grant does not cover the bind, before anything was
/// bound; [`Error::Timeout`] when no answer came within [`ForwardSettings::answer_timeout`]; [`Error::Closed`] when the stream
/// ended with no answer.
pub fn listen(
    link: &dyn Open,
    bind: &Bind,
    backlog: Option<u32>,
    settings: &ForwardSettings,
) -> Result<RemoteListener, Error> {
    let mut request = ListenRequest::new(bind.clone());
    request.backlog = backlog;
    let (stream, frame) = open_first(
        link,
        bohio_proto::protocol::LISTEN,
        &request.to_metadata()?,
        settings,
    )?;
    match ListenFrame::decode(&frame) {
        Ok(ListenFrame::Listening(listening)) => Ok(RemoteListener {
            stream,
            address: listening.address,
            token: listening.token,
        }),
        Ok(ListenFrame::Error(body)) => {
            stream.close();
            Err(Error::Remote(body))
        }
        Ok(_) | Err(_) => {
            stream.close();
            Err(Error::Protocol(bohio_proto::Error::Decode))
        }
    }
}

impl RemoteListener {
    /// What the host bound, in [`Bind`]'s spelling: a TCP address with the port the host chose, `unix:PATH` or `pipe:NAME`.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The listener's token, which names it in `bohio-accept/1`.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Waits up to `timeout` (`None`: until something happens) for the next connection the host took.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host reports the listener failed; [`Error::Transport`] when the stream failed or the host
    /// revoked it; [`Error::Closed`] when the host ended it.
    pub fn next_arrival(&self, timeout: Option<Duration>) -> Result<Option<Arrival>, Error> {
        match self.stream.receive(timeout) {
            Ok(Some(frame)) => match ListenFrame::decode(&frame) {
                Ok(ListenFrame::Arrival(arrival)) => Ok(Some(arrival)),
                Ok(ListenFrame::Error(body)) => Err(Error::Remote(body)),
                Ok(_) | Err(_) => Err(Error::Protocol(bohio_proto::Error::Decode)),
            },
            Ok(None) => Err(Error::Closed),
            Err(Failure::Timeout) => Ok(None),
            Err(failure) => Err(Error::Transport(failure)),
        }
    }

    /// Claims `arrival`, one connection this listener took, and gives its bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] with `not_found` when the host no longer holds it (it was claimed, it expired after `[forward]
    /// accept_timeout` or the listener ended), or `busy`; otherwise as [`connect`](super::connect).
    pub fn accept(
        &self,
        link: &dyn Open,
        arrival: &Arrival,
        settings: &ForwardSettings,
    ) -> Result<Tunnel, Error> {
        let request = AcceptRequest {
            token: self.token.clone(),
            id: arrival.id,
        };
        open_answered(
            link,
            bohio_proto::protocol::ACCEPT,
            &request.to_metadata()?,
            settings,
        )
    }

    /// Whether the host's listener is still there.
    pub fn is_open(&self) -> bool {
        self.stream.life() == cabuya::mux::StreamLife::Open
    }
}

impl Drop for RemoteListener {
    fn drop(&mut self) {
        self.stream.close();
    }
}

/// A connection from this machine to what an arrival should reach.
enum Local {
    Tcp(TcpStream),
    Ipc(coqui_transport_pipe::Stream),
}

/// Connects to `target` on this machine: TCP, a Unix socket (Unix) or a named pipe (Windows).
///
/// # Errors
///
/// The operating system's error, or `Unsupported` for a kind this machine has not.
pub fn connect_local(target: &Target, settings: &ForwardSettings) -> io::Result<LocalConnection> {
    let options = &settings.options;
    let local = match target {
        Target::Tcp { host, port } => {
            let mut last = None;
            let addresses = (host.as_str(), *port).to_socket_addrs()?;
            let mut connected = None;
            for address in addresses {
                match TcpStream::connect_timeout(&address, options.connect_timeout) {
                    Ok(connection) => {
                        connected = Some(connection);
                        break;
                    }
                    Err(error) => last = Some(error),
                }
            }
            Local::Tcp(connected.ok_or_else(|| {
                last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address"))
            })?)
        }
        #[cfg(unix)]
        Target::Unix { path } => Local::Ipc(ipc_connect(PipeName::at(path), settings)?),
        #[cfg(windows)]
        Target::Pipe { name } => Local::Ipc(ipc_connect(
            PipeName::at(format!(r"\\.\pipe\{name}")),
            settings,
        )?),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this machine has no such kind of socket",
            ));
        }
    };
    Ok(LocalConnection(local))
}

fn ipc_connect(
    name: Result<PipeName, coqui_transport_pipe::Error>,
    settings: &ForwardSettings,
) -> io::Result<coqui_transport_pipe::Stream> {
    let name = name.map_err(io::Error::other)?;
    ipc::connect(&name, settings.options.connect_timeout, &settings.options).map_err(|error| {
        match error {
            coqui_transport_pipe::Error::NotListening => {
                io::Error::new(io::ErrorKind::ConnectionRefused, error)
            }
            coqui_transport_pipe::Error::AccessDenied => {
                io::Error::new(io::ErrorKind::PermissionDenied, error)
            }
            other => io::Error::other(other),
        }
    })
}

/// A connection made by [`connect_local`], ready for [`Tunnel::pump_local`].
pub struct LocalConnection(Local);

impl core::fmt::Debug for LocalConnection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalConnection").finish_non_exhaustive()
    }
}

impl Tunnel {
    /// Moves bytes between this tunnel and `connection` until both directions end, then closes the tunnel: see
    /// [`Tunnel::pump_tcp`] and [`Tunnel::pump_ipc`].
    pub fn pump_local(self, connection: LocalConnection) {
        match connection.0 {
            Local::Tcp(connection) => self.pump_tcp(&connection),
            Local::Ipc(connection) => self.pump_ipc(connection),
        }
    }
}

/// `ssh -R`: a host listener whose every arrival is connected to a target on this machine and bridged.
///
/// The accepting runs on a thread of this struct's own. Dropping it, or [`RemoteForward::close`], closes the listener
/// stream, which ends the host's listener and resets the connections in flight.
pub struct RemoteForward {
    address: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Ended>>,
    ended: Arc<Mutex<Option<Ended>>>,
}

impl core::fmt::Debug for RemoteForward {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RemoteForward")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl RemoteForward {
    /// Binds `bind` on the host and starts bridging its arrivals to `local`. A connection that cannot be bridged (the local
    /// target refused, the host no longer held the arrival) is closed, and the reason goes to `report`.
    ///
    /// # Errors
    ///
    /// As [`listen`], and `InvalidInput` through [`Error::Link`] when `settings.options` has a zero.
    pub fn open(
        link: Arc<dyn Open>,
        bind: &Bind,
        local: &Target,
        settings: &ForwardSettings,
        report: Arc<dyn Fn(&Error) + Send + Sync>,
    ) -> Result<Self, Error> {
        settings
            .options
            .validate()
            .map_err(|error| Error::Link(error.to_string()))?;
        let listener = listen(&*link, bind, None, settings)?;
        let address = listener.address().to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(Mutex::new(None));
        let thread = thread::spawn({
            let (stop, ended) = (Arc::clone(&stop), Arc::clone(&ended));
            let (local, settings) = (local.clone(), *settings);
            move || {
                let why = serve(&link, &listener, &local, &settings, &stop, &report);
                *ended.lock().unwrap_or_else(PoisonError::into_inner) = Some(why);
                drop(listener);
                why
            }
        });
        Ok(Self {
            address,
            stop,
            thread: Some(thread),
            ended,
        })
    }

    /// What the host bound, as [`RemoteListener::address`].
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Why the forward ended, or `None` while it runs.
    pub fn ended(&self) -> Option<Ended> {
        *self.ended.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for the forward to end by itself: the link is lost or the host ended the listener.
    pub fn wait(mut self) -> Ended {
        self.thread
            .take()
            .and_then(|thread| thread.join().ok())
            .unwrap_or(Ended::LinkLost)
    }

    /// Ends the forward and waits for its thread.
    pub fn close(mut self) {
        self.stop_now();
    }

    fn stop_now(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RemoteForward {
    fn drop(&mut self) {
        self.stop_now();
    }
}

/// The accept loop: one thread per arrival, at most `max_connections` at once.
fn serve(
    link: &Arc<dyn Open>,
    listener: &RemoteListener,
    local: &Target,
    settings: &ForwardSettings,
    stop: &AtomicBool,
    report: &Arc<dyn Fn(&Error) + Send + Sync>,
) -> Ended {
    let live = Arc::new(AtomicUsize::new(0));
    thread::scope(|scope| {
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ended::Stopped;
            }
            if !link.is_up() {
                return Ended::LinkLost;
            }
            match listener.next_arrival(Some(settings.options.listen_poll)) {
                Ok(None) => {}
                Ok(Some(arrival)) => {
                    let over = live.load(Ordering::SeqCst) >= settings.max_connections.max(1);
                    live.fetch_add(1, Ordering::SeqCst);
                    let live = Arc::clone(&live);
                    scope.spawn(move || {
                        let result =
                            bridge_arrival(&**link, listener, &arrival, local, settings, over);
                        if let Err(error) = result {
                            report(&error);
                        }
                        live.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(error) => {
                    report(&error);
                    // A link that ends fails its streams a moment before it says it is down, so the failure counts too:
                    // only a refusal, a revoke, an error frame or an orderly end of the stream is the host ending the
                    // listener while the link stays.
                    let lost = !link.is_up()
                        || matches!(&error, Error::Transport(failure) if *failure != Failure::Permission);
                    return if lost { Ended::LinkLost } else { Ended::Closed };
                }
            }
        }
    })
}

/// Connects one arrival to the local target and carries its bytes. An arrival that cannot be bridged is still claimed and
/// dropped at once, so whoever connected to the host sees it close instead of waiting out `accept_timeout`.
fn bridge_arrival(
    link: &dyn Open,
    listener: &RemoteListener,
    arrival: &Arrival,
    local: &Target,
    settings: &ForwardSettings,
    over: bool,
) -> Result<(), Error> {
    let connection = if over {
        Err(Error::Link("too many forwarded connections".to_owned()))
    } else {
        connect_local(local, settings).map_err(|error| {
            Error::Link(format!(
                "the local target did not connect: {}",
                error.kind()
            ))
        })
    };
    match connection {
        Ok(connection) => {
            let tunnel = listener.accept(link, arrival, settings)?;
            tunnel.pump_local(connection);
            Ok(())
        }
        Err(error) => {
            if let Ok(tunnel) = listener.accept(link, arrival, settings) {
                tunnel.into_stream().close();
            }
            Err(error)
        }
    }
}
