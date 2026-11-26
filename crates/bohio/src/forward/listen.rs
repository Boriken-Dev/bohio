//! Remote forwards: the host listens for a peer and hands it each connection it takes.
//!
//! [`serve_listen`] is `bohio-listen/1`: the grant decides the bind when the stream opens ([`covers_listen`]) and at every
//! re-check, the host binds, answers with the listener's address and token, and then announces each connection that arrives.
//! The listener lives exactly as long as that stream: when the peer closes it, the link ends or the grant stops covering the
//! bind, the listener closes, the connections still waiting are closed and the ones already handed over are reset.
//! [`serve_accept`] is `bohio-accept/1`: the peer claims one waiting connection by the listener's token and the connection's
//! id, and the host carries its bytes over that stream as `bohio-connect/1` does.
//!
//! A claim names a listener of the claiming peer only: a token that is unknown, another peer's, already claimed or expired is
//! answered the same way (`not_found`), so a peer learns nothing about another's listeners.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use bohio_client::forward::ipc;
use bohio_proto::forward::{AcceptRequest, Arrival, Bind, ListenFrame, ListenRequest, Listening};
use bohio_proto::{ErrorBody, ErrorCode};
use cabuya::mux::{CloseReason, LinkStream, StreamLife};
use coqui_transport_pipe::{Error as PipeError, Listener as PipeListener, PipeName};

use super::{Context, answer_connected, hold, refuse_with, send_waiting};
use crate::config::ForwardConfig;
use crate::limit::{Counted, Slots};
use crate::link::{ForwardKind, ForwardOutcome};
use crate::policy::Grant;

/// The kinds of listener this host binds, as `info` lists them: TCP everywhere, a Unix socket on Unix and a named pipe on
/// Windows.
pub fn listen_kinds() -> Vec<String> {
    let mut kinds = vec!["tcp".to_owned()];
    kinds.push(if cfg!(windows) { "pipe" } else { "unix" }.to_owned());
    kinds
}

/// Whether `grant` still covers the bind a `bohio-listen/1` open asked for. A request that cannot be read is covered, so the
/// engine answers it with `bad_request`.
pub fn covers_listen(grant: &Grant, metadata: &[u8]) -> bool {
    ListenRequest::from_metadata(metadata)
        .map_or(true, |request| grant.permits_listen(&request.bind))
}

/// The host's listeners: which exist, whose they are and what each holds waiting.
#[derive(Debug)]
pub struct Listeners {
    by_token: Mutex<HashMap<String, Arc<Entry>>>,
    slots: Slots,
    per_peer: Counted<String>,
}

impl Listeners {
    /// The registry for `config`'s `max_listeners` and `max_listeners_per_peer`.
    pub fn new(config: &ForwardConfig) -> Self {
        Self {
            by_token: Mutex::new(HashMap::new()),
            slots: Slots::new(config.max_listeners),
            per_peer: Counted::new(config.max_listeners_per_peer),
        }
    }

    /// How many listeners are bound now.
    pub fn len(&self) -> usize {
        self.map().len()
    }

    /// Whether no listener is bound.
    pub fn is_empty(&self) -> bool {
        self.map().is_empty()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Entry>>> {
        self.by_token.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes arrival `id` of listener `token` when `peer` owns it.
    fn claim(&self, token: &str, peer: &str, id: u64) -> Option<(Arc<Entry>, Waiting)> {
        let entry = Arc::clone(self.map().get(token)?);
        if entry.owner != peer || !entry.alive.load(Ordering::SeqCst) {
            return None;
        }
        let waiting = entry.pending().remove(&id)?;
        Some((entry, waiting))
    }
}

/// One listener.
#[derive(Debug)]
struct Entry {
    owner: String,
    address: String,
    alive: AtomicBool,
    next: AtomicU64,
    waiting: Mutex<HashMap<u64, Waiting>>,
}

impl Entry {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Waiting>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A connection the listener took that nobody has claimed yet.
#[derive(Debug)]
struct Waiting {
    connection: Incoming,
    source: Option<String>,
    since: Instant,
}

#[derive(Debug)]
enum Incoming {
    Tcp(TcpStream),
    Ipc(coqui_transport_pipe::Stream),
}

/// What a listener holds while it runs: its entry in the registry and a slot of each bound. Dropping it ends the listener's
/// connections and makes its token unknown.
struct Registered<'a> {
    listeners: &'a Listeners,
    token: String,
    entry: Arc<Entry>,
    _slot: crate::limit::Permit<'a>,
    _peer: crate::limit::Hold<'a, String>,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.entry.alive.store(false, Ordering::SeqCst);
        self.listeners.map().remove(&self.token);
        self.entry.pending().clear();
    }
}

/// A bound listener of either family.
enum Bound {
    Tcp(TcpListener),
    Ipc(PipeListener),
}

impl Bound {
    /// The next connection, waiting at most `poll` for it.
    fn next(&self, poll: Duration) -> io::Result<Option<(Incoming, Option<String>)>> {
        match self {
            Self::Tcp(listener) => match listener.accept() {
                Ok((connection, source)) => {
                    connection.set_nonblocking(false)?;
                    Ok(Some((Incoming::Tcp(connection), Some(source.to_string()))))
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    thread::sleep(poll);
                    Ok(None)
                }
                // A connection that was reset before it was accepted is not the listener's failure.
                Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => Ok(None),
                Err(error) => Err(error),
            },
            Self::Ipc(listener) => match listener.accept_timeout(Some(poll)) {
                Ok(connection) => Ok(Some((Incoming::Ipc(connection), None))),
                Err(PipeError::TimedOut) => Ok(None),
                Err(error) => Err(error.into()),
            },
        }
    }
}

/// Whether `connection` is from the account this host runs as. A connection whose account the system does not name is not.
fn same_user(connection: &coqui_transport_pipe::Stream) -> bool {
    connection
        .peer()
        .ok()
        .and_then(|identity| identity.is_current_user().ok().flatten())
        .unwrap_or(false)
}

/// Serves one `bohio-listen/1` stream to its end. The stream is not closed.
pub fn serve_listen(stream: &dyn LinkStream, metadata: &[u8], context: &Context<'_>) {
    let request = match ListenRequest::from_metadata(metadata) {
        Ok(request) => request,
        Err(_) => {
            return refuse_listen(
                stream,
                context,
                "?",
                ErrorBody::new(ErrorCode::BadRequest, "that is not a listen request"),
            );
        }
    };
    let text = request.bind.to_string();
    let config = context.config;
    if !config.enabled {
        return refuse_listen(
            stream,
            context,
            &text,
            ErrorBody::new(
                ErrorCode::Unsupported,
                "this host does not forward connections",
            ),
        );
    }
    if !context.grant.permits_listen(&request.bind) {
        return refuse_listen(
            stream,
            context,
            &text,
            ErrorBody::new(ErrorCode::Denied, "this peer may not listen there"),
        );
    }
    let listeners = context.listeners;
    let peer = context.peer.to_string();
    let (Some(slot), Some(held_by_peer)) = (
        listeners.slots.acquire(),
        listeners.per_peer.take(peer.clone()),
    ) else {
        return refuse_listen(
            stream,
            context,
            &text,
            ErrorBody::new(ErrorCode::Busy, "too many listeners"),
        );
    };
    let options = config.options();
    let (bound, address) = match bind(&request.bind, config, context.grant, &options) {
        Ok(pair) => pair,
        Err(body) => return refuse_listen(stream, context, &text, body),
    };
    let token = match token() {
        Some(token) => token,
        None => {
            return refuse_listen(
                stream,
                context,
                &text,
                ErrorBody::new(ErrorCode::Internal, "the host has no source of randomness"),
            );
        }
    };
    let entry = Arc::new(Entry {
        owner: peer,
        address: address.clone(),
        alive: AtomicBool::new(true),
        next: AtomicU64::new(1),
        waiting: Mutex::new(HashMap::new()),
    });
    listeners.map().insert(token.clone(), Arc::clone(&entry));
    let _registered = Registered {
        listeners,
        token: token.clone(),
        entry: Arc::clone(&entry),
        _slot: slot,
        _peer: held_by_peer,
    };
    let listening = ListenFrame::Listening(Listening {
        address: address.clone(),
        token,
    });
    let Ok(frame) = listening.encode() else {
        return;
    };
    if !send_waiting(stream, &frame) {
        return;
    }
    context.event(ForwardKind::Listen, &address, ForwardOutcome::Connected);
    let limit = request
        .backlog
        .map_or(config.max_pending, |backlog| {
            config
                .max_pending
                .min(usize::try_from(backlog).unwrap_or(usize::MAX))
        })
        .max(1);
    if let Err(error) = run(stream, &bound, &entry, config, limit) {
        let body = io_body(&error);
        if let Ok(frame) = ListenFrame::Error(body).encode() {
            send_waiting(stream, &frame);
        }
        let _ = stream.finish();
    }
}

/// The listener's loop: takes connections and announces them until the stream ends or the listener fails.
fn run(
    stream: &dyn LinkStream,
    bound: &Bound,
    entry: &Entry,
    config: &ForwardConfig,
    limit: usize,
) -> io::Result<()> {
    let poll = config.listen_poll.0;
    let wait = config.accept_timeout.0;
    while stream.life() == StreamLife::Open {
        entry
            .pending()
            .retain(|_, waiting| waiting.since.elapsed() < wait);
        let Some((connection, source)) = bound.next(poll)? else {
            continue;
        };
        if let Incoming::Ipc(connection) = &connection
            && config.ipc_same_user
            && !same_user(connection)
        {
            continue;
        }
        let id = entry.next.fetch_add(1, Ordering::SeqCst);
        {
            let mut waiting = entry.pending();
            if waiting.len() >= limit {
                continue;
            }
            waiting.insert(
                id,
                Waiting {
                    connection,
                    source: source.clone(),
                    since: Instant::now(),
                },
            );
        }
        let Ok(frame) = ListenFrame::Arrival(Arrival { id, source }).encode() else {
            return Ok(());
        };
        if !send_waiting(stream, &frame) {
            return Ok(());
        }
    }
    Ok(())
}

/// Serves one `bohio-accept/1` stream to its end. The stream is not closed.
pub fn serve_accept(stream: &dyn LinkStream, metadata: &[u8], context: &Context<'_>) {
    let Ok(request) = AcceptRequest::from_metadata(metadata) else {
        return refuse_with(
            stream,
            ErrorBody::new(ErrorCode::BadRequest, "that is not an accept request"),
        );
    };
    let not_found = || ErrorBody::new(ErrorCode::NotFound, "no such arrival");
    if !context.config.enabled {
        return refuse_with(
            stream,
            ErrorBody::new(
                ErrorCode::Unsupported,
                "this host does not forward connections",
            ),
        );
    }
    let peer = context.peer.to_string();
    let Some((entry, waiting)) = context.listeners.claim(&request.token, &peer, request.id) else {
        context.event(
            ForwardKind::Accept,
            "?",
            ForwardOutcome::Failed(ErrorCode::NotFound),
        );
        return refuse_with(stream, not_found());
    };
    let address = entry.address.clone();
    let Some(_held) = hold(context) else {
        context.event(
            ForwardKind::Accept,
            &address,
            ForwardOutcome::Failed(ErrorCode::Busy),
        );
        return refuse_with(
            stream,
            ErrorBody::new(ErrorCode::Busy, "too many forwarded connections"),
        );
    };
    if !answer_connected(stream, waiting.source.clone()) {
        return;
    }
    context.event(ForwardKind::Accept, &address, ForwardOutcome::Connected);
    let options = context.config.options();
    let done = AtomicBool::new(false);
    let watch = context.config.watch.0;
    thread::scope(|scope| {
        // The listener ending ends its connections; the pump below blocks on the stream, so a watcher closes it.
        scope.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                if !entry.alive.load(Ordering::SeqCst) {
                    stream.close_with(CloseReason::Refused, "the listener ended");
                    return;
                }
                thread::sleep(watch);
            }
        });
        match waiting.connection {
            Incoming::Tcp(connection) => {
                cabuya::forward::pump_with(stream, &connection, &options);
            }
            Incoming::Ipc(connection) => ipc::pump(stream, connection, &options),
        }
        done.store(true, Ordering::SeqCst);
    });
}

fn refuse_listen(stream: &dyn LinkStream, context: &Context<'_>, bind: &str, body: ErrorBody) {
    context.event(ForwardKind::Listen, bind, ForwardOutcome::Failed(body.code));
    if let Ok(frame) = ListenFrame::Error(body).encode() {
        send_waiting(stream, &frame);
    }
    let _ = stream.finish();
}

/// Binds what `request` names; the listener and its address in [`Bind`]'s spelling.
#[cfg_attr(not(unix), allow(unused_variables))]
fn bind(
    request: &Bind,
    config: &ForwardConfig,
    grant: &Grant,
    options: &cabuya::forward::Options,
) -> Result<(Bound, String), ErrorBody> {
    match request {
        Bind::Tcp { host, port } => {
            let address: IpAddr = host.parse().map_err(|_| {
                ErrorBody::new(ErrorCode::BadRequest, "a bind address is an IP address")
            })?;
            let listener = TcpListener::bind(SocketAddr::new(address, *port))
                .map_err(|error| bind_failure(&error))?;
            listener
                .set_nonblocking(true)
                .map_err(|error| bind_failure(&error))?;
            let bound = listener
                .local_addr()
                .map_err(|error| bind_failure(&error))?;
            let text = Bind::Tcp {
                host: bound.ip().to_string(),
                port: bound.port(),
            }
            .to_string();
            Ok((Bound::Tcp(listener), text))
        }
        #[cfg(unix)]
        Bind::Unix { path } => {
            let at = if config.follow_links {
                path.clone()
            } else {
                resolved_bind(path, grant)?
            };
            bind_ipc(PipeName::at(at), request, config, options)
        }
        #[cfg(windows)]
        Bind::Pipe { name } => bind_ipc(
            PipeName::at(format!(r"\\.\pipe\{name}")),
            request,
            config,
            options,
        ),
        _ => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "this host does not offer that kind of listener",
        )),
    }
}

/// Where a socket the peer asked for would be made once its directory is resolved: the same path, or one a rule of the
/// peer's grant also covers. A link in the directory's path that leads elsewhere is refused, as the rule was written for
/// the text.
#[cfg(unix)]
fn resolved_bind(path: &str, grant: &Grant) -> Result<String, ErrorBody> {
    let as_path = std::path::Path::new(path);
    let (Some(parent), Some(name)) = (as_path.parent(), as_path.file_name()) else {
        return Ok(path.to_owned());
    };
    if parent.as_os_str().is_empty() {
        return Ok(path.to_owned());
    }
    let parent = std::fs::canonicalize(parent).map_err(|_| {
        ErrorBody::new(ErrorCode::NotFound, "the socket's directory does not exist")
    })?;
    let resolved = parent.join(name);
    let denied = || ErrorBody::new(ErrorCode::Denied, "this peer may not bind there");
    let text = resolved.to_str().ok_or_else(denied)?;
    if text != path
        && !grant.permits_listen(&Bind::Unix {
            path: text.to_owned(),
        })
    {
        return Err(denied());
    }
    Ok(text.to_owned())
}

fn bind_ipc(
    name: Result<PipeName, PipeError>,
    request: &Bind,
    config: &ForwardConfig,
    options: &cabuya::forward::Options,
) -> Result<(Bound, String), ErrorBody> {
    let name = name
        .map_err(|_| ErrorBody::new(ErrorCode::BadRequest, "that is not a socket or pipe name"))?;
    let listener = ipc::listener(&name, options, Some(config.socket_mode.0))
        .map_err(|error| pipe_bind_failure(&error))?;
    if config.socket_mode.0 != 0o600 {
        tracing::info!(mode = ?config.socket_mode, "a socket listener is made with a mode other than 0o600");
    }
    Ok((Bound::Ipc(listener), request.to_string()))
}

fn pipe_bind_failure(error: &PipeError) -> ErrorBody {
    match error {
        PipeError::NameTaken | PipeError::NotASocket => ErrorBody::new(
            ErrorCode::InUse,
            "that socket or pipe name is already in use",
        ),
        PipeError::AccessDenied
        | PipeError::SharedDirectory
        | PipeError::ForeignDirectory
        | PipeError::GroupDenied => ErrorBody::new(
            ErrorCode::Denied,
            "the host's account may not create that socket or pipe there",
        ),
        PipeError::InvalidName => {
            ErrorBody::new(ErrorCode::BadRequest, "that is not a socket or pipe name")
        }
        PipeError::Unsupported => ErrorBody::new(
            ErrorCode::Unsupported,
            "this host does not offer that kind of listener",
        ),
        PipeError::Io(error) if error.kind() == io::ErrorKind::NotFound => {
            ErrorBody::new(ErrorCode::NotFound, "the socket's directory does not exist")
        }
        _ => ErrorBody::new(ErrorCode::Io, "the listener could not be made"),
    }
}

fn bind_failure(error: &io::Error) -> ErrorBody {
    match error.kind() {
        io::ErrorKind::AddrInUse => {
            ErrorBody::new(ErrorCode::InUse, "that address and port are already in use")
        }
        io::ErrorKind::PermissionDenied => {
            ErrorBody::new(ErrorCode::Denied, "the operating system refused the bind")
        }
        io::ErrorKind::AddrNotAvailable => ErrorBody::new(
            ErrorCode::Unreachable,
            "this host has no such address to bind",
        ),
        _ => ErrorBody::new(ErrorCode::Io, "the listener could not be made"),
    }
}

fn io_body(_: &io::Error) -> ErrorBody {
    ErrorBody::new(ErrorCode::Io, "the listener failed")
}

/// A random 128-bit value in hex.
fn token() -> Option<String> {
    use std::fmt::Write as _;
    let mut bytes = [0_u8; 16];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .ok()?;
    Some(
        bytes
            .iter()
            .fold(String::with_capacity(32), |mut text, byte| {
                let _ = write!(text, "{byte:02x}");
                text
            }),
    )
}
