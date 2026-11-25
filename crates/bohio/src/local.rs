//! The local control endpoint of a running node, and its client.
//!
//! A node holds links to peers, including links a peer dialed, and the only process that can open streams on
//! them is the node. [`Local`] is the endpoint it listens on (a pipe: a Windows named pipe or a Unix
//! socket, this user alone by default); [`Node::serve_local`] serves two protocols there, never on a peer's
//! link: [`bohio_proto::local::VIA`] carries a stream of any other protocol to a peer, and
//! [`bohio_proto::local::LINKS`] lists the links, [`bohio_proto::local::PROVIDERS`] lists the provider libraries loaded at start, and [`bohio_proto::local::PAIR`] arms the node's pairing PIN. The mux on this endpoint is not sealed: who may connect is
//! the boundary, as for the key file.
//!
//! [`Via`] is the client: an [`Open`] whose streams run on a node's link to one peer, so `info`, `exec`, `read`
//! and `shell` work over a link the peer made.

#![forbid(unsafe_code)]

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bohio_client::connect::Opener;
use bohio_client::dial::Endpoint;
use bohio_client::link::Settings;
use bohio_client::relay::relay;
use bohio_client::{Error, Open};
use bohio_proto::gui::{ApproveInput, ApproveOutput, PendingRequest};
use bohio_proto::local::{
    GUI_APPROVE, LINKS, LibraryEntry, LinkEntry, LinksReply, LinksRequest, PAIR, PROVIDERS,
    PairReply, PairRequest, ProgramEntry, ProvidersReply, VIA, ViaRequest,
};
use cabuya::link::{Accepted, Host, Options, Services, serve_with};
use cabuya::mux::{CloseReason, LinkStream, Mux, Side, StreamEnd, StreamLife};
use cabuya::peers::{NoEvents, PeerRecord, PeerStore};
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure};
use coqui_transport_pipe::{Listener as PipeListener, SocketGroup};

use crate::config::ControlConfig;
use crate::link::Grants;
use crate::node::{Node, NodeError, Registry, Side as LinkSide};
use crate::pair::{PairEnd, PairState};

/// The bound local endpoint of a node.
#[derive(Debug)]
pub struct Local {
    listener: PipeListener,
    endpoint: Endpoint,
    check_user: bool,
}

impl Local {
    /// Binds the endpoint `config` names for the node `node`.
    ///
    /// # Errors
    ///
    /// [`NodeError::Listen`] for an endpoint that is not a `pipe://` one or cannot be bound.
    pub fn bind(config: &ControlConfig, node: &PeerId) -> Result<Self, NodeError> {
        let text = config.endpoint_for(node);
        let fail = |reason: String| NodeError::Listen {
            endpoint: text.clone(),
            reason,
        };
        let Endpoint::Pipe(name) = Endpoint::parse(&text).map_err(fail)? else {
            return Err(fail("the control endpoint is a `pipe://` one".to_owned()));
        };
        let mut builder = PipeListener::builder().name(&name);
        if let Some(bits) = config.socket_mode_bits().map_err(fail)? {
            builder = builder.socket_mode(bits);
        }
        if let Some(group) = &config.socket_group {
            let group: SocketGroup =
                group
                    .parse()
                    .map_err(|error: coqui_transport_pipe::Error| {
                        fail(format!("socket_group: {error}"))
                    })?;
            builder = builder.socket_group(Some(group));
        }
        if let Some(descriptor) = &config.security_descriptor {
            builder = builder.security_descriptor(Some(descriptor.clone()));
        }
        let listener = builder.build().map_err(|error| fail(error.to_string()))?;
        Ok(Self {
            listener,
            endpoint: Endpoint::Pipe(name),
            check_user: !config.sets_access(),
        })
    }

    /// The endpoint as bound.
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Waits up to `wait` for a client; one that is not this user (when the operator set nothing about who
    /// may connect) is closed unread.
    fn accept(&self, wait: Duration, settings: &Settings) -> io::Result<Option<Box<dyn Channel>>> {
        match self.listener.accept_timeout(Some(wait)) {
            Ok(connection) => {
                if self.check_user {
                    let same = connection
                        .peer()
                        .and_then(|who| who.is_current_user())
                        .ok()
                        .flatten();
                    if same != Some(true) {
                        tracing::info!("a local client that is not this user was closed unread");
                        return Ok(None);
                    }
                }
                let channel = bohio_client::link::stream(connection, settings)?;
                Ok(Some(Box::new(channel)))
            }
            Err(coqui_transport_pipe::Error::TimedOut) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

/// What the two local protocols read.
struct Context {
    pairing: Arc<PairState>,
    registry: Arc<Registry>,
    grants: Arc<dyn Grants>,
    settings: Settings,
    approvals: Arc<crate::gui::Approvals>,
    sources: Arc<crate::access::Sources>,
}

/// The one local peer: it may open both local protocols.
struct Everything;

impl PeerStore for Everything {
    fn get(&self, peer: &str) -> Result<Option<PeerRecord>, cabuya::Error> {
        Ok(Some(
            PeerRecord::new(peer)
                .granting(VIA)
                .granting(LINKS)
                .granting(PROVIDERS)
                .granting(PAIR)
                .granting(GUI_APPROVE),
        ))
    }

    fn put(&self, _record: &PeerRecord) -> Result<(), cabuya::Error> {
        Ok(())
    }

    fn remove(&self, _peer: &str) -> Result<(), cabuya::Error> {
        Ok(())
    }
}

impl Node {
    /// Serves clients of `local` until `stop` is set, each on a thread of its own.
    ///
    /// # Errors
    ///
    /// The listener's error.
    pub fn serve_local(&self, local: &Local, stop: &AtomicBool) -> io::Result<()> {
        let (_, _, settings) = self.local_parts();
        thread::scope(|scope| {
            while !stop.load(Ordering::SeqCst) {
                let Some(channel) = local.accept(settings.accept_wait, &settings)? else {
                    continue;
                };
                scope.spawn(move || {
                    self.serve_local_channel(&channel, Some(stop));
                    channel.close();
                });
            }
            Ok(())
        })
    }

    /// Serves one local client on `channel` (an unproven, framed channel, as
    /// [`bohio_client::link::stream`] makes) until it ends, or `stop` is set.
    pub fn serve_local_channel<C: Channel>(&self, channel: &C, stop: Option<&AtomicBool>) {
        let (registry, grants, settings) = self.local_parts();
        let context = Arc::new(Context {
            pairing: self.pair_state(),
            registry,
            grants,
            settings,
            approvals: self.gui_approvals(),
            sources: self.sources(),
        });
        channel.set_max_message(settings.max_frame.saturating_add(5));
        let _ = channel.set_pending(settings.read_ahead);
        let services = Services::default()
            .with(
                VIA,
                Box::new({
                    let context = Arc::clone(&context);
                    move |stream: &dyn LinkStream, accepted: &Accepted| {
                        via(&context, stream, accepted);
                    }
                }),
            )
            .with(
                LINKS,
                Box::new({
                    let context = Arc::clone(&context);
                    move |stream: &dyn LinkStream, accepted: &Accepted| {
                        links(&context, stream, accepted);
                    }
                }),
            )
            .with(
                PROVIDERS,
                Box::new({
                    let context = Arc::clone(&context);
                    move |stream: &dyn LinkStream, accepted: &Accepted| {
                        providers(&context, stream, accepted);
                    }
                }),
            )
            .with(
                GUI_APPROVE,
                Box::new({
                    let context = Arc::clone(&context);
                    move |stream: &dyn LinkStream, accepted: &Accepted| {
                        approve(&context, stream, accepted);
                    }
                }),
            )
            .with(
                PAIR,
                Box::new({
                    let context = Arc::clone(&context);
                    move |stream: &dyn LinkStream, accepted: &Accepted| {
                        pair(&context, stream, accepted);
                    }
                }),
            );
        let host = Host {
            store: &Everything,
            services: &services,
            events: &NoEvents,
        };
        let mut options = Options::default();
        options.limits = settings.limits();
        let finished = AtomicBool::new(false);
        thread::scope(|scope| {
            if let Some(stop) = stop {
                scope.spawn(|| {
                    while !finished.load(Ordering::SeqCst) {
                        if stop.load(Ordering::SeqCst) {
                            channel.close();
                            return;
                        }
                        thread::sleep(settings.poll_interval);
                    }
                });
            }
            serve_with(
                channel,
                Side::Answered,
                options,
                None,
                "local",
                &host,
                |_| {},
            );
            finished.store(true, Ordering::SeqCst);
        });
    }
}

/// Moves messages from `from` to `to` until `from` ends or `done` is set. A `finish` of `from` is passed on and only this
/// direction ends (the other goes on, which is how a half-close crosses); any other end of either stream sets `done`.
fn pump(from: &dyn LinkStream, to: &dyn LinkStream, done: &AtomicBool, poll: Duration) {
    while !done.load(Ordering::SeqCst) {
        match from.receive(Some(poll)) {
            Ok(Some(message)) => {
                if to.send(&message).is_err() {
                    return done.store(true, Ordering::SeqCst);
                }
            }
            Ok(None) if from.finished_by_peer() => {
                if to.finish().is_err() {
                    done.store(true, Ordering::SeqCst);
                }
                return;
            }
            Err(Failure::Timeout) => {}
            Ok(None) | Err(_) => return done.store(true, Ordering::SeqCst),
        }
    }
}

/// `bohio-via/1`: opens the named protocol on the node's link to the peer and passes messages both ways
/// unchanged. The peer's refusal or revoke closes the local stream with the same reason and text.
fn via(context: &Context, stream: &dyn LinkStream, accepted: &Accepted) {
    let Ok(request) = ViaRequest::from_metadata(&accepted.metadata) else {
        return stream.close_with(CloseReason::Refused, "that is not a via request");
    };
    let Ok(peer) = request.peer.parse::<PeerId>() else {
        return stream.close_with(CloseReason::Refused, "that is not a peer id");
    };
    let Some(link) = context.registry.find(&peer) else {
        return stream.close_with(CloseReason::Refused, "no link to that peer");
    };
    let remote = match link.open_stream(&request.protocol, &request.metadata) {
        Ok(remote) => remote,
        Err(error) => {
            return stream.close_with(
                CloseReason::Refused,
                &format!("the link to that peer cannot open it: {error}"),
            );
        }
    };
    let poll = context.settings.poll_interval.max(Duration::from_millis(1));
    let done = AtomicBool::new(false);
    let finished = AtomicUsize::new(0);
    thread::scope(|scope| {
        let local = relay(scope, stream, &context.settings);
        let (done, finished, remote) = (&done, &finished, &remote);
        {
            let local = Arc::clone(&local);
            scope.spawn(move || {
                pump(&*local, remote, done, poll);
                finished.fetch_add(1, Ordering::SeqCst);
            });
        }
        {
            let local = Arc::clone(&local);
            scope.spawn(move || {
                pump(remote, &*local, done, poll);
                finished.fetch_add(1, Ordering::SeqCst);
            });
        }
        // Both directions end in order (each passed its finish on), or either fails or ends otherwise.
        while !done.load(Ordering::SeqCst) && finished.load(Ordering::SeqCst) < 2 {
            thread::sleep(poll);
        }
        done.store(true, Ordering::SeqCst);
        match remote.end() {
            Some(StreamEnd::Refused(text)) => stream.close_with(CloseReason::Refused, &text),
            Some(StreamEnd::Revoked(text)) => stream.close_with(CloseReason::Revoked, &text),
            Some(StreamEnd::Superseded(text)) => {
                stream.close_with(CloseReason::Superseded, &text);
            }
            Some(StreamEnd::Broken) => {
                stream.close_with(CloseReason::Broken, "the peer's stream broke its bounds");
            }
            Some(StreamEnd::LinkLost(_)) => {
                stream.close_with(CloseReason::Broken, "the link to that peer was lost");
            }
            _ => {}
        }
        local.close();
        remote.close();
    });
}

/// `bohio-links/1`: one reply listing the links, then the end.
fn links(context: &Context, stream: &dyn LinkStream, accepted: &Accepted) {
    let Ok(request) = LinksRequest::from_metadata(&accepted.metadata) else {
        return stream.close_with(CloseReason::Refused, "that is not a links request");
    };
    let wait = request
        .wait_ms
        .map_or(context.settings.proof_timeout, Duration::from_millis);
    let listed = context.registry.list_links(context.grants.as_ref());
    let entries: Vec<LinkEntry> = thread::scope(|scope| {
        let reads: Vec<_> = listed
            .iter()
            .map(|(_, link)| scope.spawn(move || link.offers(wait).ok()))
            .collect();
        listed
            .iter()
            .zip(reads)
            .map(|((info, _), read)| LinkEntry {
                peer: info.peer.to_string(),
                side: match info.side {
                    LinkSide::Dialed => "dialed",
                    _ => "answered",
                }
                .to_owned(),
                endpoint: info.endpoint.clone(),
                source: info.source.map(|source| source.to_string()),
                since: info
                    .since
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs()),
                mine: sorted(info.mine.clone()),
                theirs: read.join().ok().flatten().map(sorted),
            })
            .collect()
    });
    let Ok(reply) = (LinksReply { links: entries }).encode() else {
        return stream.close_with(CloseReason::Broken, "the links cannot be written");
    };
    let _ = stream.send(&reply);
    let _ = stream.finish();
    let _ = stream.receive(Some(context.settings.send_limit));
}

/// `bohio-providers/1`: the provider libraries the node loaded at start, answered once.
fn providers(context: &Context, stream: &dyn LinkStream, _accepted: &Accepted) {
    let report = context.sources.libraries();
    let reply = ProvidersReply {
        check_access: report.check_access,
        libraries: report
            .libraries
            .into_iter()
            .map(|library| LibraryEntry {
                path: library.path.display().to_string(),
                state: library.state.name().to_owned(),
                detail: library.detail,
                digest: library.digest,
            })
            .collect(),
        programs: report
            .programs
            .into_iter()
            .map(|program| ProgramEntry {
                name: program.name,
                path: program.path.display().to_string(),
                state: program.state.name().to_owned(),
                detail: program.detail,
                digest: program.digest,
                restarts: program.restarts,
            })
            .collect(),
    };
    let Ok(message) = reply.encode() else {
        return stream.close_with(CloseReason::Broken, "the providers cannot be written");
    };
    let _ = stream.send(&message);
    let _ = stream.finish();
    let _ = stream.receive(Some(context.settings.send_limit));
}

/// `bohio-gui-approve/1`: the desktop requests waiting for the owner, sent at once and again whenever they change, and the
/// answer to each decision the client sends.
fn approve(context: &Context, stream: &dyn LinkStream, _accepted: &Accepted) {
    let poll = context.settings.poll_interval.max(Duration::from_millis(1));
    let send = |output: &ApproveOutput| {
        output
            .encode()
            .ok()
            .is_some_and(|message| stream.send(&message).is_ok())
    };
    let mut version = context.approvals.version();
    if !send(&ApproveOutput::Pending {
        requests: context.approvals.list(),
    }) {
        return;
    }
    loop {
        match stream.receive(Some(poll)) {
            Ok(Some(message)) => {
                let Ok(ApproveInput::Decide { id, allow }) = ApproveInput::decode(&message) else {
                    return stream.close_with(CloseReason::Refused, "that is not a decision");
                };
                let found = context.approvals.decide(id, allow);
                if !send(&ApproveOutput::Decided { id, found }) {
                    return;
                }
            }
            Err(Failure::Timeout) => {}
            Ok(None) | Err(_) => return,
        }
        let now = context.approvals.version();
        if now != version {
            version = now;
            if !send(&ApproveOutput::Pending {
                requests: context.approvals.list(),
            }) {
                return;
            }
        }
    }
}

/// `bohio-pair/1`: arms a PIN, says it, then says how it ended. A client that goes away withdraws the PIN.
fn pair(context: &Context, stream: &dyn LinkStream, accepted: &Accepted) {
    let Ok(request) = PairRequest::from_metadata(&accepted.metadata) else {
        return stream.close_with(CloseReason::Refused, "that is not a pair request");
    };
    let send = |reply: &PairReply| {
        if let Ok(message) = reply.encode() {
            let _ = stream.send(&message);
        }
    };
    let armed = match context
        .pairing
        .arm(request.lifetime_ms.map(Duration::from_millis))
    {
        Ok(armed) => armed,
        Err(error) => {
            send(&PairReply::Ended {
                reason: error.to_string(),
            });
            let _ = stream.finish();
            return;
        }
    };
    send(&PairReply::Armed {
        pin: armed.pin().to_owned(),
        lifetime_ms: u64::try_from(armed.lifetime().as_millis()).unwrap_or(u64::MAX),
    });
    let poll = context.settings.poll_interval.max(Duration::from_millis(1));
    let end = loop {
        if let Some(end) = armed.wait(poll) {
            break end;
        }
        if stream.life() != StreamLife::Open {
            armed.withdraw();
            return;
        }
    };
    send(&match end {
        PairEnd::Paired(peer) => PairReply::Paired {
            peer: peer.to_string(),
        },
        other => PairReply::Ended {
            reason: other.to_string(),
        },
    });
    let _ = stream.finish();
    let _ = stream.receive(Some(context.settings.send_limit));
}

/// Who a client accepts as the server of a local endpoint. The endpoint is not sealed, so the account that serves
/// the name is the only thing that says it is the node: a name another account bound first (while the node was
/// down, or so that its bind failed) would read every request and answer as it liked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Server {
    /// This account alone, as a node serves its endpoint when `[control]` sets nothing about who may connect.
    ThisAccount,
    /// Any account, for an endpoint whose access the operator set (`[control] socket_mode`, `socket_group` or
    /// `security_descriptor`), where the node may run as another account: who may bind the name is then the check.
    Any,
}

impl Server {
    /// The server a client of the endpoint `config` describes accepts.
    pub fn for_control(config: &ControlConfig) -> Self {
        if config.sets_access() {
            Self::Any
        } else {
            Self::ThisAccount
        }
    }
}

/// The text of the error a client gets from an endpoint another account serves.
pub const ANOTHER_ACCOUNT: &str =
    "the local endpoint is served by another account, so it is not this node's";

/// Opens the pipe `endpoint` names and checks who serves it.
fn dial_local(
    endpoint: &Endpoint,
    settings: &Settings,
    server: Server,
) -> Result<coqui_transport::StreamChannel, Error> {
    use coqui_transport::Connector as _;
    let Endpoint::Pipe(name) = endpoint else {
        return Err(Error::Link(
            "a local endpoint is a `pipe://` one".to_owned(),
        ));
    };
    let pipe = coqui_transport_pipe::Connector::new(name.clone())
        .connect(settings.proof_timeout)
        .map_err(|_| Error::Transport(Failure::PathLost))?;
    if server == Server::ThisAccount {
        let same = pipe
            .peer()
            .and_then(|who| who.is_current_user())
            .ok()
            .flatten();
        if same != Some(true) {
            return Err(Error::Link(ANOTHER_ACCOUNT.to_owned()));
        }
    }
    bohio_client::link::stream(pipe, settings).map_err(|_| Error::Transport(Failure::PathLost))
}

/// A client of a node's local endpoint: the links it holds, and [`Via`] their streams.
#[derive(Debug)]
pub struct LocalLink {
    opener: Opener,
    thread: Mutex<Option<JoinHandle<()>>>,
    settings: Settings,
}

impl LocalLink {
    /// Connects to the local endpoint `endpoint` within `settings.proof_timeout`, served by this account.
    ///
    /// # Errors
    ///
    /// As [`LocalLink::connect_to`] with [`Server::ThisAccount`].
    pub fn connect(endpoint: &Endpoint, settings: &Settings) -> Result<Self, Error> {
        Self::connect_to(endpoint, settings, Server::ThisAccount)
    }

    /// Connects to the local endpoint `endpoint` within `settings.proof_timeout`, accepting `server` as the one
    /// that serves it.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when nothing answers there (no node runs, or it serves no control endpoint);
    /// [`Error::Link`] for an endpoint that is not a `pipe://` one, or one another account serves when `server`
    /// is [`Server::ThisAccount`] (the text is [`ANOTHER_ACCOUNT`]), before anything is sent to it.
    pub fn connect_to(
        endpoint: &Endpoint,
        settings: &Settings,
        server: Server,
    ) -> Result<Self, Error> {
        let channel = dial_local(endpoint, settings, server)?;
        channel.set_max_message(settings.max_frame.saturating_add(5));
        let _ = channel.set_pending(settings.read_ahead);
        let (opener, opens) = Opener::pair(settings);
        let settings = *settings;
        let (ready, ready_rx) = std::sync::mpsc::channel();
        let thread = thread::Builder::new()
            .name("bohio-local".to_owned())
            .spawn(move || {
                Mux::run(&channel, Side::Dialed, settings.limits(), None, |mux| {
                    let _ = ready.send(());
                    opens.run(mux, &settings);
                });
                channel.close();
            })
            .map_err(|_| Error::Transport(Failure::Cancelled))?;
        ready_rx.recv().map_err(|_| Error::Closed)?;
        Ok(Self {
            opener,
            thread: Mutex::new(Some(thread)),
            settings,
        })
    }

    /// Asks the node to arm a PIN lasting `lifetime` (`[pairing] timeout` when `None`), which a joiner may try on any of
    /// the node's listeners.
    ///
    /// # Errors
    ///
    /// The stream's failure; [`Error::Link`] with the node's reason when it cannot arm one (it has no store to pair into).
    pub fn pair(&self, lifetime: Option<Duration>) -> Result<Pairing, Error> {
        let request = PairRequest {
            lifetime_ms: lifetime.map(|time| u64::try_from(time.as_millis()).unwrap_or(u64::MAX)),
        };
        let stream = self.opener.open(PAIR, &request.to_metadata()?)?;
        let message = stream
            .receive(Some(self.settings.proof_timeout))?
            .ok_or(Error::Closed)?;
        match PairReply::decode(&message).map_err(|error| Error::Link(error.to_string()))? {
            PairReply::Armed { pin, lifetime_ms } => Ok(Pairing {
                stream,
                pin,
                lifetime: Duration::from_millis(lifetime_ms),
                settings: self.settings,
            }),
            PairReply::Ended { reason } => Err(Error::Link(reason)),
            PairReply::Paired { .. } => {
                Err(Error::Link("the node paired before it armed".to_owned()))
            }
        }
    }

    /// The desktop requests the node waits for the owner to allow or deny.
    ///
    /// # Errors
    ///
    /// The stream's failure, or [`Error::Link`] for a reply that is not one.
    pub fn gui_pending(&self) -> Result<Vec<PendingRequest>, Error> {
        let stream = self.opener.open(GUI_APPROVE, &[])?;
        let message = stream
            .receive(Some(self.settings.proof_timeout))?
            .ok_or(Error::Closed)?;
        stream.close();
        match ApproveOutput::decode(&message).map_err(|error| Error::Link(error.to_string()))? {
            ApproveOutput::Pending { requests } => Ok(requests),
            ApproveOutput::Decided { .. } => Err(Error::Link(
                "the node answered a decision nobody sent".to_owned(),
            )),
        }
    }

    /// Allows or denies request `id`; `false` when it is not waiting any more (it timed out, or was decided).
    ///
    /// # Errors
    ///
    /// As [`LocalLink::gui_pending`].
    pub fn gui_decide(&self, id: u64, allow: bool) -> Result<bool, Error> {
        let stream = self.opener.open(GUI_APPROVE, &[])?;
        let wait = self.settings.proof_timeout;
        stream.receive(Some(wait))?.ok_or(Error::Closed)?;
        let message = ApproveInput::Decide { id, allow }
            .encode()
            .map_err(|error| Error::Link(error.to_string()))?;
        stream.send_timeout(&message, Some(wait))?;
        loop {
            let reply = stream.receive(Some(wait))?.ok_or(Error::Closed)?;
            match ApproveOutput::decode(&reply).map_err(|error| Error::Link(error.to_string()))? {
                ApproveOutput::Decided {
                    id: answered,
                    found,
                } if answered == id => {
                    stream.close();
                    return Ok(found);
                }
                _ => {}
            }
        }
    }

    /// The node's links, each with what the peer offers read just now (waiting up to `wait` for each).
    ///
    /// # Errors
    ///
    /// The stream's failure, or [`Error::Link`] for a reply that is not one.
    pub fn links(&self, wait: Duration) -> Result<LinksReply, Error> {
        let request = LinksRequest {
            wait_ms: Some(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)),
        };
        let stream = self.opener.open(LINKS, &request.to_metadata()?)?;
        let until = wait.saturating_add(self.settings.proof_timeout);
        let message = stream.receive(Some(until))?.ok_or(Error::Closed)?;
        stream.close();
        LinksReply::decode(&message).map_err(|error| Error::Link(error.to_string()))
    }

    /// The provider libraries the node loaded at start, and what became of each.
    ///
    /// # Errors
    ///
    /// The stream's failure, or [`Error::Link`] for a reply that is not one.
    pub fn providers(&self) -> Result<ProvidersReply, Error> {
        let stream = self.opener.open(PROVIDERS, &[])?;
        let message = stream
            .receive(Some(self.settings.proof_timeout))?
            .ok_or(Error::Closed)?;
        stream.close();
        ProvidersReply::decode(&message).map_err(|error| Error::Link(error.to_string()))
    }
}

impl Drop for LocalLink {
    fn drop(&mut self) {
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

/// A PIN a node armed for a client that asked through its local endpoint. Dropping it before it ends withdraws the PIN.
pub struct Pairing {
    stream: Box<dyn Channel>,
    pin: String,
    lifetime: Duration,
    settings: Settings,
}

impl Pairing {
    /// The PIN, for the person who reads it out.
    pub fn pin(&self) -> &str {
        &self.pin
    }

    /// How long the node holds it.
    pub const fn lifetime(&self) -> Duration {
        self.lifetime
    }

    /// Waits for the PIN to end: the peer that paired with it, or why nobody did.
    ///
    /// # Errors
    ///
    /// The stream's failure, or [`Error::Link`] for a reply that is not one.
    pub fn wait(self) -> Result<PairReply, Error> {
        let until = self.lifetime.saturating_add(self.settings.proof_timeout);
        let message = self.stream.receive(Some(until))?.ok_or(Error::Closed)?;
        self.stream.close();
        PairReply::decode(&message).map_err(|error| Error::Link(error.to_string()))
    }
}

impl core::fmt::Debug for Pairing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pairing")
            .field("lifetime", &self.lifetime)
            .finish_non_exhaustive()
    }
}

impl Drop for Pairing {
    fn drop(&mut self) {
        self.stream.close();
    }
}

/// An [`Open`] whose streams run on a node's link to one peer. The peer's grants for the node decide what
/// they may do; a peer the node has no link to refuses them (a [`Failure::Permission`] on the first read).
#[derive(Debug)]
pub struct Via {
    link: LocalLink,
    peer: PeerId,
}

impl Via {
    /// Connects to the local endpoint `endpoint`, served by this account, for streams on the node's link to `peer`.
    ///
    /// # Errors
    ///
    /// As [`LocalLink::connect`].
    pub fn connect(endpoint: &Endpoint, peer: &PeerId, settings: &Settings) -> Result<Self, Error> {
        Self::connect_to(endpoint, peer, settings, Server::ThisAccount)
    }

    /// [`Via::connect`], accepting `server` as the one that serves the endpoint.
    ///
    /// # Errors
    ///
    /// As [`LocalLink::connect_to`].
    pub fn connect_to(
        endpoint: &Endpoint,
        peer: &PeerId,
        settings: &Settings,
        server: Server,
    ) -> Result<Self, Error> {
        Ok(Self {
            link: LocalLink::connect_to(endpoint, settings, server)?,
            peer: *peer,
        })
    }

    /// The node's links, as [`LocalLink::links`].
    ///
    /// # Errors
    ///
    /// As [`LocalLink::links`].
    pub fn links(&self, wait: Duration) -> Result<LinksReply, Error> {
        self.link.links(wait)
    }
}

impl Open for Via {
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        let request = ViaRequest {
            peer: self.peer.to_string(),
            protocol: protocol.to_owned(),
            metadata: metadata.to_vec(),
        };
        self.link.opener.open(VIA, &request.to_metadata()?)
    }

    fn is_up(&self) -> bool {
        self.link.opener.is_up()
    }
}

/// The time a link came up, as the local reply gives it.
pub fn since(entry: &LinkEntry) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(entry.since)
}

fn sorted(mut offers: Vec<String>) -> Vec<String> {
    offers.sort();
    offers
}
