//! Forwarding: the host connects to a target for a peer and carries its bytes over the peer's stream.
//!
//! [`serve_connect`] is `bohio-connect/1`: the grant decided the request when the stream opened ([`covers`]) and decides it
//! again at every re-check, so a stream that loses its grant is reset. The host connects, answers once (`connected`, or an
//! error that says why nothing was connected), then moves raw bytes both ways. [`serve_tcp`] is `tcp/1` as `cabuya`
//! defines it, under the same grant: the connection is made before any answer, and a refusal is a close.
//!
//! The pump is `cabuya::forward::pump_with` for TCP and [`bohio_client::forward::ipc::pump`] for a Unix socket or a
//! Windows named pipe, both over the stream itself with no relay in between.
//!
//! [`serve_listen`] and [`serve_accept`] are the other direction (`bohio-listen/1` and `bohio-accept/1`): the host listens for
//! the peer and hands it each connection, which the peer claims on a stream of its own and which then moves as above.

#![forbid(unsafe_code)]

mod keep;
mod listen;
mod rules;
mod spec;

use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs as _};
use std::time::Duration;

use bohio_client::forward::ipc;
use bohio_proto::forward::{Answer, ConnectRequest, Connected, Target};
use bohio_proto::{ErrorBody, ErrorCode};
use cabuya::mux::{CloseReason, LinkStream, StreamLife};
use coqui_peer::PeerId;
use coqui_transport::Failure;
use coqui_transport_pipe::PipeName;

use crate::config::ForwardConfig;
use crate::limit::{Counted, Slots};
use crate::link::{ForwardKind, ForwardOutcome, NodeEvent, NodeEvents};
use crate::policy::Grant;

pub use keep::{Declarations, Declared, RemoteKeep, keep_remote};
pub(crate) use keep::{Keeper, run as keep_forwards, same_port};
pub use listen::{Listeners, covers_listen, listen_kinds, serve_accept, serve_listen};
pub use rules::{Addresses, BadRule, ConnectRule, ListenRule, PortSet, tcp_target};
pub use spec::{BadSpec, Bound, LocalSpec, RemoteSpec, SocksSpec};

/// The kinds of target this host connects to, as `info` lists them: TCP everywhere, a Unix socket on Unix and a named pipe on
/// Windows.
pub fn connect_kinds() -> Vec<String> {
    let mut kinds = vec!["tcp".to_owned()];
    kinds.push(if cfg!(windows) { "pipe" } else { "unix" }.to_owned());
    kinds
}

/// What [`serve_connect`] and [`serve_tcp`] need besides the stream.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    /// The peer's grant.
    pub grant: &'a Grant,
    /// The `[forward]` settings.
    pub config: &'a ForwardConfig,
    /// The forwarded connections open across peers: `[forward] max_connections`.
    pub slots: &'a Slots,
    /// The forwarded connections each peer has open: `[forward] max_connections_per_peer`.
    pub per_peer: &'a Counted<String>,
    /// The remote forwards' listeners.
    pub listeners: &'a Listeners,
    /// The peer.
    pub peer: PeerId,
    /// Where `forwarded` events go.
    pub events: &'a dyn NodeEvents,
}

impl core::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

/// Whether `grant` still covers the target a `bohio-connect/1` open asked for. A request that cannot be read is covered, so
/// the engine answers it with `bad_request`.
pub fn covers(grant: &Grant, metadata: &[u8]) -> bool {
    ConnectRequest::from_metadata(metadata)
        .map_or(true, |request| grant.permits_connect(&request.target))
}

/// As [`covers`], for the `HOST:PORT` text a `tcp/1` open carries.
pub fn covers_tcp(grant: &Grant, metadata: &[u8]) -> bool {
    match core::str::from_utf8(metadata).map(str::parse::<Target>) {
        Ok(Ok(target @ Target::Tcp { .. })) => grant.permits_connect(&target),
        // Not a target: the engine refuses it.
        _ => true,
    }
}

/// Serves one `bohio-connect/1` stream to its end. The stream is not closed.
pub fn serve_connect(stream: &dyn LinkStream, metadata: &[u8], context: &Context<'_>) {
    let request = match ConnectRequest::from_metadata(metadata) {
        Ok(request) => request,
        Err(_) => {
            return refuse(
                stream,
                context,
                "?",
                ErrorBody::new(ErrorCode::BadRequest, "that is not a connect request"),
            );
        }
    };
    let target = request.target;
    let text = target.to_string();
    if !context.config.enabled {
        return refuse(
            stream,
            context,
            &text,
            ErrorBody::new(
                ErrorCode::Unsupported,
                "this host does not forward connections",
            ),
        );
    }
    if !context.grant.permits_connect(&target) {
        return refuse(
            stream,
            context,
            &text,
            ErrorBody::new(ErrorCode::Denied, "this peer may not reach that target"),
        );
    }
    let Some(_held) = hold(context) else {
        return refuse(
            stream,
            context,
            &text,
            ErrorBody::new(ErrorCode::Busy, "too many forwarded connections"),
        );
    };
    let options = context.config.options();
    match target {
        Target::Tcp { .. } => {
            let connected = connect_tcp(&target, context.config.connect_timeout.0);
            let (connection, address) = match connected {
                Ok(pair) => pair,
                Err(body) => return refuse(stream, context, &text, body),
            };
            if !answer_connected(stream, Some(address.to_string())) {
                return;
            }
            context.event(ForwardKind::Connect, &text, ForwardOutcome::Connected);
            cabuya::forward::pump_with(stream, &connection, &options);
        }
        Target::Unix { .. } | Target::Pipe { .. } => {
            let connection = match connect_ipc(&target, &options) {
                Ok(connection) => connection,
                Err(body) => return refuse(stream, context, &text, body),
            };
            if !answer_connected(stream, None) {
                return;
            }
            context.event(ForwardKind::Connect, &text, ForwardOutcome::Connected);
            ipc::pump(stream, connection, &options);
        }
        // A kind this version does not know cannot have been decoded.
        _ => refuse(
            stream,
            context,
            &text,
            ErrorBody::new(ErrorCode::Unsupported, "that kind of target is not served"),
        ),
    }
}

/// Serves one `tcp/1` stream to its end, as `cabuya` does: no answer before the bytes, and a refusal or a failed connect
/// is a close with a reason. The stream is not closed.
pub fn serve_tcp(stream: &dyn LinkStream, metadata: &[u8], context: &Context<'_>) {
    let target = match core::str::from_utf8(metadata).map(str::parse::<Target>) {
        Ok(Ok(target @ Target::Tcp { .. })) => target,
        _ => return stream.close_with(CloseReason::Refused, "that is not a target"),
    };
    let text = target.to_string();
    if !context.config.enabled || !context.grant.permits_connect(&target) {
        context.event(
            ForwardKind::Connect,
            &text,
            ForwardOutcome::Failed(ErrorCode::Denied),
        );
        return stream.close_with(CloseReason::Refused, "this peer may not reach that");
    }
    let Some(_held) = hold(context) else {
        context.event(
            ForwardKind::Connect,
            &text,
            ForwardOutcome::Failed(ErrorCode::Busy),
        );
        return stream.close_with(CloseReason::Refused, "too many forwarded connections");
    };
    match connect_tcp(&target, context.config.connect_timeout.0) {
        Ok((connection, _)) => {
            context.event(ForwardKind::Connect, &text, ForwardOutcome::Connected);
            cabuya::forward::pump_with(stream, &connection, &context.config.options());
        }
        Err(body) => {
            context.event(
                ForwardKind::Connect,
                &text,
                ForwardOutcome::Failed(body.code),
            );
            stream.close_with(CloseReason::Refused, "that target cannot be reached");
        }
    }
}

/// What a connection holds while it runs: a slot of each bound.
struct Held<'a> {
    _slot: crate::limit::Permit<'a>,
    _peer: crate::limit::Hold<'a, String>,
}

fn hold<'a>(context: &Context<'a>) -> Option<Held<'a>> {
    let slot = context.slots.acquire()?;
    let peer = context.per_peer.take(context.peer.to_string())?;
    Some(Held {
        _slot: slot,
        _peer: peer,
    })
}

impl Context<'_> {
    fn event(&self, kind: ForwardKind, target: &str, outcome: ForwardOutcome) {
        self.events.event(&NodeEvent::Forwarded {
            peer: self.peer,
            kind,
            target: target.to_owned(),
            outcome,
        });
    }
}

/// Tells the peer nothing was connected, and ends the stream.
fn refuse(stream: &dyn LinkStream, context: &Context<'_>, target: &str, body: ErrorBody) {
    context.event(
        ForwardKind::Connect,
        target,
        ForwardOutcome::Failed(body.code),
    );
    refuse_with(stream, body);
}

/// As [`refuse`], with no event.
fn refuse_with(stream: &dyn LinkStream, body: ErrorBody) {
    if let Ok(frame) = (Answer::Error(body)).encode() {
        send_waiting(stream, &frame);
    }
    let _ = stream.finish();
}

/// Sends the `connected` frame; `false` when the stream is gone.
fn answer_connected(stream: &dyn LinkStream, address: Option<String>) -> bool {
    match (Answer::Connected(Connected { address })).encode() {
        Ok(frame) => send_waiting(stream, &frame),
        Err(_) => false,
    }
}

/// How long a send that failed at once waits before it is tried again: not a bound on anything, only what keeps a retry
/// that fails without waiting from spinning a core.
const RETRY_PAUSE: Duration = Duration::from_millis(1);

/// Sends one small frame, waiting while the link has no credit for it; `false` when the stream ended first.
fn send_waiting(stream: &dyn LinkStream, frame: &[u8]) -> bool {
    loop {
        match stream.send(frame) {
            Ok(()) => return true,
            Err(Failure::NoProgress | Failure::Timeout) if stream.life() == StreamLife::Open => {
                std::thread::sleep(RETRY_PAUSE);
            }
            Err(_) => return false,
        }
    }
}

/// Connects to a TCP target, each address its name resolves to up to `timeout`.
fn connect_tcp(target: &Target, timeout: Duration) -> Result<(TcpStream, SocketAddr), ErrorBody> {
    let Some(coqui) = tcp_target(target) else {
        return Err(ErrorBody::new(
            ErrorCode::BadRequest,
            "that is not a target",
        ));
    };
    // Resolved here, not by `Target::connect_within`, so a name that does not resolve is told apart from one that refuses.
    let addresses: Vec<SocketAddr> = match coqui.to_string().to_socket_addrs() {
        Ok(addresses) => addresses.collect(),
        Err(_) => {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                "the name does not resolve",
            ));
        }
    };
    let mut last = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(connection) => return Ok((connection, address)),
            Err(error) => last = Some(error),
        }
    }
    Err(last.map_or_else(
        || ErrorBody::new(ErrorCode::NotFound, "the name has no address"),
        |error| io_failure(&error),
    ))
}

/// Connects to a Unix socket or a named pipe.
fn connect_ipc(
    target: &Target,
    options: &cabuya::forward::Options,
) -> Result<coqui_transport_pipe::Stream, ErrorBody> {
    let name = match target {
        #[cfg(unix)]
        Target::Unix { path } => {
            if !std::path::Path::new(path).exists() {
                return Err(ErrorBody::new(ErrorCode::NotFound, "no such socket"));
            }
            PipeName::at(path)
        }
        #[cfg(windows)]
        Target::Pipe { name } => PipeName::at(format!(r"\\.\pipe\{name}")),
        _ => {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "this host does not offer that kind of target",
            ));
        }
    };
    let name = name.map_err(|error| ErrorBody::new(ErrorCode::BadRequest, error.to_string()))?;
    ipc::connect(&name, options.connect_timeout, options).map_err(|error| pipe_failure(&error))
}

fn pipe_failure(error: &coqui_transport_pipe::Error) -> ErrorBody {
    use coqui_transport_pipe::Error as Pipe;
    match error {
        Pipe::NotListening => ErrorBody::new(
            if cfg!(unix) {
                ErrorCode::Refused
            } else {
                ErrorCode::NotFound
            },
            "nothing listens there",
        ),
        Pipe::AccessDenied => ErrorBody::new(
            ErrorCode::Denied,
            "the host's account may not connect there",
        ),
        Pipe::TimedOut => ErrorBody::new(ErrorCode::Timeout, "the target did not answer in time"),
        Pipe::Unsupported => ErrorBody::new(
            ErrorCode::Unsupported,
            "this host does not offer that kind of target",
        ),
        Pipe::InvalidName => {
            ErrorBody::new(ErrorCode::BadRequest, "that is not a socket or pipe name")
        }
        _ => ErrorBody::new(ErrorCode::Io, "the connection to the target failed"),
    }
}

/// The code a failed connect is reported with, and a sentence that names no address.
fn io_failure(error: &io::Error) -> ErrorBody {
    let (code, message) = match error.kind() {
        io::ErrorKind::ConnectionRefused => {
            (ErrorCode::Refused, "the target refused the connection")
        }
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
            (ErrorCode::Timeout, "the target did not answer in time")
        }
        io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::NetworkDown
        | io::ErrorKind::AddrNotAvailable => {
            (ErrorCode::Unreachable, "the target cannot be reached")
        }
        io::ErrorKind::NotFound => (ErrorCode::NotFound, "the target was not found"),
        io::ErrorKind::PermissionDenied => (
            ErrorCode::Denied,
            "the operating system refused the connection",
        ),
        _ => (ErrorCode::Io, "the connection to the target failed"),
    };
    ErrorBody::new(code, message)
}
