//! The node: one embeddable host that proves each peer, decides what it may do and runs its streams.
//!
//! A [`Node`] is made from what its caller supplies: a [`NodeConfig`], an identity, the accepted
//! [`Keys`] and the [`Grants`], and optionally [`NodeEvents`] and the host's `PATH`. It reads no file,
//! no environment variable and no process-wide state, so an application builds one from settings kept
//! wherever it likes; `bohio serve` is one such application.
//!
//! A connection proves itself first, within `[link] proof_timeout`, holding an unproven peer to one
//! small message. A key that proves itself and that [`Keys`] does not accept is offered to
//! [`Keys::stranger`] and the connection ends. An accepted peer's link is then served: each stream
//! it opens is decided by [`crate::link`] at open and at every `[link] recheck`, and runs one engine
//! behind a bounded [`Relay`](bohio_client::relay::Relay).

use std::collections::HashMap;
use std::ffi::OsString;
use std::hash::Hash;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

pub use bohio_client::dial::Endpoint;
use bohio_client::dial::{SerialOptions, serial};
use bohio_client::link::Settings;
use bohio_client::relay::relay;
use bohio_proto::info::Info;
use bohio_proto::{ErrorBody, ErrorCode, protocol};
use coqui_link::auth::{Identity, Proof, Sealed, Trust, prove};
use coqui_link::link::{Accepted as Open, Host, Options, Services, serve as serve_link};
use coqui_link::mux::{CloseReason, LinkStream, Side};
use coqui_peer::{Handshake, KeyMaterial, Method as ProofMethod, PeerId};
use coqui_transport::Channel;

use crate::config::ConfigError;
pub use crate::config::NodeConfig;
use crate::limit::Slots;
use crate::link::{Bridge, NodePeers, PROTOCOLS, admission, grant_of};
pub use crate::link::{
    Grants, GrantsError, Keys, LogEvents, MemoryGrants, MemoryKeys, NodeEvent, NodeEvents,
};
use crate::listen::Listening;
use crate::policy::Grant;
use crate::{control, exec, fs, info, shell};

/// How long a listener waits for a connection before it looks at whether it was stopped.
const ACCEPT_WAIT: Duration = Duration::from_millis(200);

/// How long a serial host leaves the port closed between links.
const SERIAL_REOPEN: Duration = Duration::from_millis(250);

/// Why a node could not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NodeError {
    /// The settings are out of range.
    #[error("{0}")]
    Config(ConfigError),
    /// The proof could not be set up.
    #[error("the proof cannot be set up: {0}")]
    Proof(String),
    /// A listener could not be opened.
    #[error("cannot listen on {endpoint}: {reason}")]
    Listen {
        /// The endpoint.
        endpoint: String,
        /// The operating system's reason.
        reason: String,
    },
}

impl From<ConfigError> for NodeError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

/// What every connection shares.
struct Shared {
    config: NodeConfig,
    settings: Settings,
    grants: Arc<dyn Grants>,
    path: OsString,
    info: Info,
    processes: Slots,
    transfers: Slots,
    sessions: Slots,
}

impl Shared {
    fn new(
        config: &NodeConfig,
        grants: &Arc<dyn Grants>,
        host: PeerId,
        path: OsString,
    ) -> Arc<Self> {
        let mut capabilities = info::collect(config, &path);
        capabilities.host_id = Some(host.to_string());
        Arc::new(Self {
            settings: config.link.settings(),
            grants: Arc::clone(grants),
            path,
            info: capabilities,
            processes: Slots::new(config.execution.max_processes),
            transfers: Slots::new(config.filesystem.max_transfers),
            sessions: Slots::new(config.shell.max_sessions),
            config: config.clone(),
        })
    }
}

/// Where a node's events go, changeable until it serves: the proof holds the cell, not one sink.
struct EventCell(RwLock<Arc<dyn NodeEvents>>);

impl NodeEvents for EventCell {
    fn event(&self, event: &NodeEvent) {
        let sink = Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner));
        sink.event(event);
    }
}

/// An embeddable host: its identity, the keys and grants its caller supplies and the engines it serves.
pub struct Node {
    shared: Arc<Shared>,
    grants: Arc<dyn Grants>,
    keys: Arc<dyn Keys>,
    events: Arc<EventCell>,
    proof: Proof,
    peers: NodePeers,
    host: PeerId,
    connections: AtomicUsize,
    /// Connections still proving themselves, by source: `[link] max_unproven_per_source` each.
    unproven: Counted<IpAddr>,
    /// Proven links, by peer: `[link] max_links_per_peer` each.
    links: Counted<String>,
}

/// How many of something each key holds, at most `limit` at once.
struct Counted<K> {
    held: Mutex<HashMap<K, usize>>,
    limit: usize,
}

impl<K: Eq + Hash + Clone> Counted<K> {
    fn new(limit: usize) -> Self {
        Self {
            held: Mutex::new(HashMap::new()),
            limit,
        }
    }

    /// One more for `key`, given back when the hold drops; `None` when `key` holds `limit`.
    fn take(&self, key: K) -> Option<Hold<'_, K>> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let count = held.entry(key.clone()).or_insert(0);
        if *count >= self.limit {
            return None;
        }
        *count += 1;
        Some(Hold { counted: self, key })
    }
}

/// One of a key's [`Counted`] holds.
struct Hold<'a, K: Eq + Hash> {
    counted: &'a Counted<K>,
    key: K,
}

impl<K: Eq + Hash> Drop for Hold<'_, K> {
    fn drop(&mut self) {
        let mut held = self
            .counted
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = held.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                held.remove(&self.key);
            }
        }
    }
}

/// The source a connection counts against: an IPv4 address (an IPv4-mapped IPv6 address is its IPv4
/// address), or the /64 an IPv6 address lies in, since one holder controls every address of a /64.
fn source_key(source: SocketAddr) -> IpAddr {
    match source.ip() {
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let bits = u128::from(address) & !u128::from(u64::MAX);
                IpAddr::V6(Ipv6Addr::from(bits))
            }
        },
        v4 @ IpAddr::V4(_) => v4,
    }
}

impl core::fmt::Debug for Node {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Node")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

/// A proof by `Proof` that tells the handshake where the peer connected from, so a key that proves
/// itself and is not accepted waits under its own source rather than a shared one.
struct Sourced<'a> {
    proof: &'a Proof,
    source: Option<SocketAddr>,
}

impl ProofMethod for Sourced<'_> {
    type Handshake = <Proof as ProofMethod>::Handshake;

    fn name(&self) -> &'static str {
        self.proof.name()
    }

    fn initiate(&self) -> Result<Self::Handshake, <Self::Handshake as Handshake>::Error> {
        self.proof.initiate()
    }

    fn respond(
        &self,
        _source: Option<SocketAddr>,
    ) -> Result<Self::Handshake, <Self::Handshake as Handshake>::Error> {
        self.proof.respond(self.source)
    }
}

impl Node {
    /// A node that proves peers with `identity`, accepts the keys `keys` says and lets each do what
    /// `grants` says, under `config`. Its events are logged and its programs are looked for on an empty
    /// `PATH` until [`Node::with_events`] and [`Node::with_path`] say otherwise.
    ///
    /// # Errors
    ///
    /// [`NodeError::Config`] when `config` is out of range, [`NodeError::Proof`] when the proof cannot
    /// be built.
    pub fn new(
        config: NodeConfig,
        identity: Identity,
        keys: Arc<dyn Keys>,
        grants: Arc<dyn Grants>,
    ) -> Result<Self, NodeError> {
        config.validate("the node settings")?;
        let host = identity.peer_id();
        let events = Arc::new(EventCell(RwLock::new(Arc::new(LogEvents))));
        let asked = {
            let (keys, events) = (Arc::clone(&keys), Arc::clone(&events));
            Trust::asked(move |key: &KeyMaterial, _answering, source| {
                let peer = key.id();
                if keys.is_accepted(&peer) {
                    return true;
                }
                keys.stranger(&peer, key, source);
                events.event(&NodeEvent::Stranger { peer, source });
                false
            })
        };
        let proof = Proof::builder(identity)
            .initiators(asked)
            .build()
            .map_err(|error| NodeError::Proof(error.to_string()))?;
        Ok(Self {
            shared: Shared::new(&config, &grants, host, OsString::new()),
            peers: NodePeers::new(Arc::clone(&keys)),
            grants,
            keys,
            events,
            proof,
            host,
            connections: AtomicUsize::new(0),
            unproven: Counted::new(config.link.max_unproven_per_source),
            links: Counted::new(config.link.max_links_per_peer),
        })
    }

    /// Sends the node's events to `events` instead of the log.
    #[must_use]
    pub fn with_events(self, events: Arc<dyn NodeEvents>) -> Self {
        *self
            .events
            .0
            .write()
            .unwrap_or_else(PoisonError::into_inner) = events;
        self
    }

    /// The host's own `PATH`: where programs and shells are looked for, and what `info` reports.
    #[must_use]
    pub fn with_path(mut self, path: OsString) -> Self {
        self.shared = Shared::new(&self.shared.config, &self.grants, self.host, path);
        self
    }

    /// One service per protocol, each admitted by the grants as they are at the time.
    fn services(&self) -> Services {
        let mut services = Services::default();
        for (token, _) in PROTOCOLS {
            let shared = Arc::clone(&self.shared);
            services = services.with_admission(
                token,
                Box::new(move |stream: &dyn LinkStream, accepted: &Open| {
                    stream_served(&shared, stream, accepted);
                }),
                admission(Arc::clone(&self.grants), token),
            );
        }
        services
    }

    /// This host's id.
    pub const fn host(&self) -> PeerId {
        self.host
    }

    /// Accepts connections on `listening` until `stop` is set, serving each on a thread of its own; at
    /// most `[link] max_connections` at once, a connection past that is closed unread. A serial line
    /// has one peer, so it is served one link at a time, the port opened again after each.
    ///
    /// # Errors
    ///
    /// The listener's error.
    pub fn serve_listening(&self, listening: &Listening, stop: &AtomicBool) -> std::io::Result<()> {
        if let Listening::Serial { path, options } = listening {
            self.serve_serial(path, options, stop);
            return Ok(());
        }
        let settings = &self.shared.settings;
        let limit = self.shared.config.link.max_connections;
        thread::scope(|scope| {
            while !stop.load(Ordering::SeqCst) {
                let Some(arrival) = listening.accept(ACCEPT_WAIT, settings)? else {
                    continue;
                };
                let proving = match arrival.source() {
                    Some(source) => match self.unproven.take(source_key(source)) {
                        Some(hold) => Some(hold),
                        None => {
                            tracing::info!(
                                "a connection was closed: its source has max_unproven_per_source connections proving themselves"
                            );
                            continue;
                        }
                    },
                    None => None,
                };
                if self.connections.fetch_add(1, Ordering::SeqCst) >= limit {
                    self.connections.fetch_sub(1, Ordering::SeqCst);
                    tracing::info!("a connection was closed: max_connections are open");
                    continue;
                }
                scope.spawn(move || {
                    match arrival.open() {
                        Ok((channel, source)) => {
                            self.serve_proving(&channel, source, proving);
                            channel.close();
                        }
                        Err(error) => tracing::info!("a connection could not be set up: {error}"),
                    }
                    self.connections.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Ok(())
        })
    }

    /// Serves links on the serial port `path` one after another until `stop` is set: the port is
    /// opened, a peer proves itself and its link is served, and when that ends (or no peer proves
    /// itself within `[link] proof_timeout`) the port is opened again.
    fn serve_serial(&self, path: &str, options: &SerialOptions, stop: &AtomicBool) {
        let settings = &self.shared.settings;
        while !stop.load(Ordering::SeqCst) {
            match serial(path, options, settings) {
                Ok(channel) => {
                    self.serve_channel(&channel, None);
                    channel.close();
                }
                Err(error) => tracing::info!("the serial port {path} cannot be opened: {error}"),
            }
            thread::sleep(SERIAL_REOPEN);
        }
    }

    /// Serves the one link on this process's standard input and output until it ends: for a program
    /// started by `docker exec -i`, a jump host or a provider that owns the pipes. Nothing else may
    /// write to standard output; diagnostics go to standard error.
    ///
    /// # Errors
    ///
    /// The error of starting the channel.
    pub fn serve_stdio(&self) -> std::io::Result<()> {
        let channel = bohio_client::link::stdio(&self.shared.settings)?;
        self.serve_channel(&channel, None);
        channel.close();
        Ok(())
    }

    /// Proves the peer on `channel` and serves its link until it ends, at most `[link]
    /// max_links_per_peer` links per peer at once. The caller closes `channel`.
    pub fn serve_channel<C: Channel>(&self, channel: &C, source: Option<SocketAddr>) {
        self.serve_proving(channel, source, None);
    }

    /// As [`Node::serve_channel`], giving `proving` back once the proof has ended either way.
    fn serve_proving<C: Channel>(
        &self,
        channel: &C,
        source: Option<SocketAddr>,
        proving: Option<Hold<'_, IpAddr>>,
    ) {
        let settings = &self.shared.settings;
        let sourced = Sourced {
            proof: &self.proof,
            source,
        };
        let established = match prove(
            &sourced,
            channel,
            settings.proof_timeout,
            settings.record_length(self.proof.method()),
        ) {
            Ok(established) => established,
            Err(error @ coqui_link::Error::Proof(_)) => {
                return tracing::info!(
                    "a key was refused: {error}; a key that proved itself and is not accepted waits for `bohio peer approve`"
                );
            }
            Err(error) => return tracing::info!("a connection ended before its proof: {error}"),
        };
        drop(proving);
        let peer = established.peer;
        let Some(_link) = self.links.take(peer.to_string()) else {
            return tracing::info!(%peer, "a link was closed: the peer has max_links_per_peer links open");
        };
        let _ = channel.set_pending(settings.read_ahead);
        let recheck = self.shared.config.link.recheck.0;
        let keys = Arc::clone(&self.keys);
        // A revoked key's streams end at the next re-check; the link itself ends at the second.
        let sealed = Sealed::new(channel, established.session)
            .with_check(move || keys.is_accepted(&peer))
            .with_check_every(recheck.saturating_mul(2));
        let mut options = Options::default();
        options.limits = settings.limits();
        options.recheck = recheck;
        let services = self.services();
        let bridge = Bridge {
            events: self.events.as_ref(),
            side: Side::Answered,
        };
        let host = Host {
            store: &self.peers,
            services: &services,
            events: &bridge,
        };
        serve_link(
            &sealed,
            Side::Answered,
            options,
            None,
            &peer.to_string(),
            &host,
        );
        sealed.close();
    }
}

/// Runs one stream's engine behind a relay, under the grant the peer has now.
fn stream_served(shared: &Shared, stream: &dyn LinkStream, accepted: &Open) {
    let grant: Grant = match grant_of(shared.grants.as_ref(), &accepted.peer) {
        Ok(grant) => grant,
        Err(error) => {
            tracing::warn!("the peer grants cannot be read: {error}");
            stream.close_with(CloseReason::Refused, "the host cannot read its peer grants");
            return;
        }
    };
    thread::scope(|scope| {
        let relay = relay(scope, stream, &shared.settings);
        let channel: Arc<dyn Channel> = relay.clone();
        let metadata = accepted.metadata.as_slice();
        let config = &shared.config;
        let files = fs::Context {
            grant: &grant,
            config: &config.filesystem,
            slots: &shared.transfers,
        };
        match accepted.protocol.as_str() {
            protocol::EXEC => {
                let context = exec::Context {
                    grant: &grant,
                    config: &config.execution,
                    slots: &shared.processes,
                    path: &shared.path,
                };
                match exec::serve(channel, metadata, &context) {
                    Ok(exit) => tracing::debug!(code = ?exit.code, "exec ended"),
                    Err(error) => tracing::info!(code = ?error.code, "exec refused"),
                }
            }
            protocol::SHELL => {
                let context = shell::Context {
                    grant: &grant,
                    config: &config.shell,
                    slots: &shared.sessions,
                    path: &shared.path,
                };
                match shell::serve(channel, metadata, &context) {
                    Ok(exit) => tracing::debug!(code = ?exit.code, "shell ended"),
                    Err(error) => tracing::info!(code = ?error.code, "shell refused"),
                }
            }
            protocol::READ => {
                if let Err(error) = fs::serve_read(channel, metadata, &files) {
                    tracing::info!(code = ?error.code, "read refused");
                }
            }
            protocol::WRITE => {
                if let Err(error) = fs::serve_write(channel, metadata, &files) {
                    tracing::info!(code = ?error.code, "write refused");
                }
            }
            protocol::CONTROL => {
                // Each call is decided by the grant as it is then, not as it was when the stream opened.
                let now = || {
                    grant_of(shared.grants.as_ref(), &accepted.peer).map_err(|_| {
                        ErrorBody::new(ErrorCode::Internal, "the host cannot read its peer grants")
                    })
                };
                let context = control::Context {
                    filesystem: files,
                    info: &shared.info,
                    current: Some(control::Current(&now)),
                };
                let answered = control::serve(&channel, &context);
                tracing::debug!(answered, "control ended");
            }
            _ => {}
        }
        relay.close();
    });
}
