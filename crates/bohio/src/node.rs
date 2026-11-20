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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use bohio_client::Open as OpenStreams;
use bohio_client::connect::Opener;
pub use bohio_client::dial::Endpoint;
use bohio_client::dial::{SerialOptions, ServerTrust, serial};
use bohio_client::link::Settings;
use bohio_client::relay::relay;
use bohio_proto::info::Info;
use bohio_proto::{ErrorBody, ErrorCode, protocol};
use coqui_link::auth::{Identity, Proof, Sealed, Session, Trust, prove, prove_out};
use coqui_link::link::{Accepted as Open, Host, Options, Services, serve_with};
pub use coqui_link::mux::Side;
use coqui_link::mux::{CloseReason, LinkStream};
use coqui_peer::{Established, Handshake, KeyMaterial, Method as ProofMethod, PeerId};
use coqui_transport::Channel;

use crate::config::ConfigError;
pub use crate::config::{DialConfig, NodeConfig};
use crate::limit::Slots;
use crate::link::{Bridge, NodePeers, PROTOCOLS, admission, grant_of, offered};
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
    /// A dial did not reach a proven link: the path could not be opened, or the peer did not prove
    /// itself as the key expected.
    #[error("cannot link to {endpoint}: {reason}")]
    Dial {
        /// The endpoint, or `channel` for a dial over a channel the caller opened.
        endpoint: String,
        /// Why, with no key, PIN or request in it.
        reason: String,
    },
    /// The peer proved itself as the key expected, and the node's [`Keys`] do not accept that key, so
    /// no stream was served; [`Keys::stranger`] heard it.
    #[error("the key {peer} proved itself and is not accepted")]
    Stranger {
        /// The proven key.
        peer: PeerId,
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

/// One link a node serves, as [`Node::links`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LinkInfo {
    /// The peer, proven.
    pub peer: PeerId,
    /// Whether this node answered or dialed the link. It decides nothing: either side opens streams
    /// and each decides what the other may do by its own grant.
    pub side: Side,
    /// The endpoint this node dialed, for a link it dialed through [`Node::connect`].
    pub endpoint: Option<String>,
    /// The peer's network address, when the node knows it: where it connected from, or the address dialed.
    pub source: Option<SocketAddr>,
    /// When the link came up.
    pub since: SystemTime,
    /// The protocol tokens this node offers the peer now: those its grant for the peer holds a permission
    /// for. Computed when asked, so it follows the grants; `offers` on the peer's side is what it reads.
    pub mine: Vec<String>,
}

/// A way to open streams on a peer's link: an [`Open`](bohio_client::Open) that is cheap to clone, from
/// [`Node::peer`]. It fails with [`bohio_client::Error::Closed`] once that link has ended.
#[derive(Clone)]
pub struct PeerLink {
    peer: PeerId,
    opener: Opener,
}

impl PeerLink {
    /// The peer this link reaches.
    pub const fn peer(&self) -> PeerId {
        self.peer
    }

    /// Opens a stream, keeping the type that says why it ended: a refusal or a revoke with the text its peer
    /// gave.
    ///
    /// # Errors
    ///
    /// As [`bohio_client::Open::open`].
    pub fn open_stream(
        &self,
        protocol: &str,
        metadata: &[u8],
    ) -> Result<bohio_client::connect::Opened, bohio_client::Error> {
        self.opener.open_stream(protocol, metadata)
    }

    /// Whether the link can still open streams.
    pub fn is_up(&self) -> bool {
        self.opener.is_up()
    }

    /// What the peer offers this node now: the tokens of its `link/1` document, read at this moment, so
    /// it follows the peer's grants for this node. A side hosts what it offers and controls what it
    /// opens; the result of a negotiation is the intersection of these offers and what a side asks for.
    /// Opening a token that is not offered is refused as it always is.
    ///
    /// # Errors
    ///
    /// [`bohio_client::Error::Closed`] when the link has ended, [`bohio_client::Error::Transport`] when
    /// the document does not come within `wait`.
    pub fn offers(&self, wait: Duration) -> Result<Vec<String>, bohio_client::Error> {
        self.opener
            .capabilities(wait)
            .map(|document| document.offers)
    }
}

impl core::fmt::Debug for PeerLink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PeerLink")
            .field("peer", &self.peer)
            .field("up", &self.opener.is_up())
            .finish()
    }
}

impl OpenStreams for PeerLink {
    fn open(
        &self,
        protocol: &str,
        metadata: &[u8],
    ) -> Result<Box<dyn Channel>, bohio_client::Error> {
        self.opener.open(protocol, metadata)
    }

    fn open_with(
        &self,
        protocol: &str,
        metadata: &[u8],
        options: &bohio_client::OpenOptions,
    ) -> Result<Box<dyn Channel>, bohio_client::Error> {
        self.opener.open_with(protocol, metadata, options)
    }

    fn bulk_window(&self) -> Option<u32> {
        self.opener.bulk_window()
    }
}

/// The links a node serves, in the order they came up.
#[derive(Default)]
pub(crate) struct Registry {
    next: AtomicU64,
    entries: Mutex<Vec<Entry>>,
}

struct Entry {
    id: u64,
    info: LinkInfo,
    opener: Opener,
}

/// A link's place in the registry, taken out when the link ends.
struct Listed<'a> {
    registry: &'a Registry,
    id: u64,
}

impl Registry {
    /// The oldest link that is up to `peer`.
    pub(crate) fn find(&self, peer: &PeerId) -> Option<PeerLink> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|entry| entry.info.peer == *peer && entry.opener.is_up())
            .map(|entry| PeerLink {
                peer: *peer,
                opener: entry.opener.clone(),
            })
    }

    /// Every link that is up, oldest first, with what `grants` gives each peer as `mine`.
    pub(crate) fn list_links(&self, grants: &dyn Grants) -> Vec<(LinkInfo, PeerLink)> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|entry| entry.opener.is_up())
            .map(|entry| {
                let mut info = entry.info.clone();
                info.mine = grant_of(grants, &info.peer.to_string())
                    .map(|grant| offered(&grant).into_iter().map(str::to_owned).collect())
                    .unwrap_or_default();
                let link = PeerLink {
                    peer: info.peer,
                    opener: entry.opener.clone(),
                };
                (info, link)
            })
            .collect()
    }

    fn add(&self, info: LinkInfo, opener: Opener) -> Listed<'_> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Entry { id, info, opener });
        Listed { registry: self, id }
    }
}

impl Drop for Listed<'_> {
    fn drop(&mut self) {
        self.registry
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|entry| entry.id != self.id);
    }
}

/// An embeddable host: its identity, the keys and grants its caller supplies and the engines it serves.
pub struct Node {
    shared: Arc<Shared>,
    grants: Arc<dyn Grants>,
    keys: Arc<dyn Keys>,
    events: Arc<EventCell>,
    proof: Proof,
    identity: Identity,
    registry: Arc<Registry>,
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
        let proof = Proof::builder(identity.clone())
            .initiators(asked)
            .build()
            .map_err(|error| NodeError::Proof(error.to_string()))?;
        Ok(Self {
            identity,
            registry: Arc::default(),
            shared: Shared::new(&config, &grants, host, OsString::new()),
            peers: NodePeers::new(Arc::clone(&keys), Arc::clone(&grants)),
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
        let _ = self.serve_proven(channel, established, Side::Answered, source, None);
    }

    /// Dials a peer over `channel`, an unproven channel the caller opened (framed as
    /// [`bohio_client::link::framed`] holds it), proves this node to `expect` and `expect` to this node,
    /// and serves the link until it ends, as an answered link is served: this node opens streams as
    /// well as answers them. `source` is the address the channel reaches, when the caller knows it.
    ///
    /// The peer must prove itself as `expect` before this node sends its own key, so a path that leads
    /// somewhere else learns nothing. The caller closes `channel`.
    ///
    /// # Errors
    ///
    /// [`NodeError::Dial`] when the peer does not prove itself as `expect` within `[link]
    /// proof_timeout`, [`NodeError::Stranger`] when it does and [`Keys`] do not accept it (the link is
    /// closed before it serves anything), [`NodeError::Proof`] when the proof cannot be built.
    /// An `Ok` means a link was served and has ended.
    pub fn connect_channel<C: Channel>(
        &self,
        channel: &C,
        expect: &PeerId,
        source: Option<SocketAddr>,
    ) -> Result<(), NodeError> {
        self.dial(channel, expect, source, None, None)
    }

    /// Opens `endpoint`, within `timeout`, and links to it as [`Node::connect_channel`] does, with
    /// `trust` checking a TLS endpoint's certificate. The link is listed with `endpoint` in
    /// [`LinkInfo::endpoint`].
    ///
    /// # Errors
    ///
    /// As [`Node::connect_channel`], and [`NodeError::Dial`] when the path cannot be opened.
    pub fn connect(
        &self,
        endpoint: &Endpoint,
        expect: &PeerId,
        trust: &ServerTrust,
        timeout: Duration,
    ) -> Result<(), NodeError> {
        self.connect_until(endpoint, expect, trust, timeout, None)
    }

    /// [`Node::connect`], ending the link when `stop` is set.
    fn connect_until(
        &self,
        endpoint: &Endpoint,
        expect: &PeerId,
        trust: &ServerTrust,
        timeout: Duration,
        stop: Option<&AtomicBool>,
    ) -> Result<(), NodeError> {
        let named = endpoint.to_string();
        let channel = endpoint
            .dial_with(timeout, &self.shared.settings, trust)
            .map_err(|error| NodeError::Dial {
                endpoint: named.clone(),
                reason: error.to_string(),
            })?;
        let source = endpoint_source(endpoint);
        let linked = self.dial(&channel, expect, source, Some(named), stop);
        channel.close();
        linked
    }

    fn dial<C: Channel>(
        &self,
        channel: &C,
        expect: &PeerId,
        source: Option<SocketAddr>,
        endpoint: Option<String>,
        stop: Option<&AtomicBool>,
    ) -> Result<(), NodeError> {
        let settings = &self.shared.settings;
        let proof = Proof::builder(self.identity.clone())
            .responders(Trust::pinned([*expect]))
            .build()
            .map_err(|error| NodeError::Proof(error.to_string()))?;
        let finished = AtomicBool::new(false);
        thread::scope(|scope| {
            if let Some(stop) = stop {
                // A stopped node ends a proof or a link that is still going: closing the channel ends both.
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
            let linked = prove_out(
                &proof,
                channel,
                settings.proof_timeout,
                settings.record_length(proof.method()),
            )
            .map_err(|error| NodeError::Dial {
                endpoint: endpoint.clone().unwrap_or_else(|| "channel".to_owned()),
                reason: match error {
                    coqui_link::Error::Proof(_) => {
                        "the peer did not prove itself as the key expected".to_owned()
                    }
                    other => other.to_string(),
                },
            })
            .and_then(|established| {
                self.serve_proven(channel, established, Side::Dialed, source, endpoint)
            });
            finished.store(true, Ordering::SeqCst);
            linked
        })
    }

    /// Keeps a link to `peer` at `endpoint` (any endpoint text `bohio remote` takes) until `stop` is set: dials it,
    /// serves the link until it ends, waits and dials again. The text is read again at every attempt, so a name is
    /// resolved again after the peer moved. A failure waits `dial.retry_min`, then that times `dial.retry_factor`
    /// for each further one up to `dial.retry_max`, each wait moved by up to `dial.retry_jitter` of itself; a link
    /// that stayed up at least `retry_min` starts the sequence again. No dial is made while an up link to `peer`
    /// exists, whichever side made it. Two links made at once (the peer dialing this node as this node dials it)
    /// both serve.
    ///
    /// The peer must prove itself as `peer` before this node sends its own key. This node reports
    /// [`NodeEvent::Dialing`] at each attempt and [`NodeEvent::DialFailed`] once per change of reason; it prints
    /// nothing. It returns when `stop` is set, having ended its link, or at once when `dial` holds a certificate
    /// hash that is not one.
    pub fn run_dial(&self, peer: &PeerId, endpoint: &str, dial: &DialConfig, stop: &AtomicBool) {
        let mut trust = ServerTrust::default();
        trust.ca_files.clone_from(&dial.ca);
        for hash in &dial.cert_hash {
            if let Err(reason) = trust.pin(hash) {
                return self.events.event(&NodeEvent::DialFailed {
                    peer: *peer,
                    endpoint: endpoint.to_owned(),
                    reason,
                    retry_in: Duration::MAX,
                });
            }
        }
        let poll = self.shared.settings.poll_interval;
        let mut failures = 0u32;
        let mut told: Option<String> = None;
        let mut jitter = Jitter::new();
        while !stop.load(Ordering::SeqCst) {
            if self.peer(peer).is_some() {
                thread::sleep(poll);
                continue;
            }
            self.events.event(&NodeEvent::Dialing {
                peer: *peer,
                endpoint: endpoint.to_owned(),
            });
            let began = Instant::now();
            let result = Endpoint::parse(endpoint)
                .and_then(|parsed| match &dial.fallback {
                    Some(text) => {
                        Endpoint::parse(text).and_then(|fallback| parsed.with_fallback(fallback))
                    }
                    None => Ok(parsed),
                })
                .map_err(|reason| NodeError::Dial {
                    endpoint: endpoint.to_owned(),
                    reason,
                })
                .and_then(|parsed| {
                    self.connect_until(&parsed, peer, &trust, dial.timeout.0, Some(stop))
                });
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let reason = match result {
                Ok(()) if began.elapsed() >= dial.retry_min.0 => {
                    failures = 0;
                    told = None;
                    None
                }
                Ok(()) => Some(
                    "the peer ended the link at once: it may not accept this node's key".to_owned(),
                ),
                Err(error) => Some(match error {
                    NodeError::Dial { reason, .. } => reason,
                    other => other.to_string(),
                }),
            };
            let wait = backoff(dial, failures, jitter.next());
            if let Some(reason) = reason {
                failures = failures.saturating_add(1);
                if told.as_ref() != Some(&reason) {
                    self.events.event(&NodeEvent::DialFailed {
                        peer: *peer,
                        endpoint: endpoint.to_owned(),
                        reason: reason.clone(),
                        retry_in: wait,
                    });
                    told = Some(reason);
                }
            }
            let until = Instant::now() + wait;
            while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                thread::sleep(poll.min(until.saturating_duration_since(Instant::now())));
            }
        }
    }

    /// Serves the link of a peer that has proved itself, on whichever side it was made: the same
    /// admission, the same re-checks and a registered opener for this node's own streams.
    fn serve_proven<C: Channel>(
        &self,
        channel: &C,
        established: Established<Session>,
        side: Side,
        source: Option<SocketAddr>,
        endpoint: Option<String>,
    ) -> Result<(), NodeError> {
        let settings = &self.shared.settings;
        let peer = established.peer;
        if !self.keys.is_accepted(&peer) {
            if let Some(key) = &established.key {
                self.keys.stranger(&peer, key, source);
            }
            self.events.event(&NodeEvent::Stranger { peer, source });
            tracing::info!(%peer, "a link was closed: the key proved itself and is not accepted");
            return Err(NodeError::Stranger { peer });
        }
        let Some(_link) = self.links.take(peer.to_string()) else {
            tracing::info!(%peer, "a link was closed: the peer has max_links_per_peer links open");
            return Ok(());
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
            side,
        };
        let host = Host {
            store: &self.peers,
            services: &services,
            events: &bridge,
        };
        let (opener, opens) = Opener::pair(settings);
        let _listed = self.registry.add(
            LinkInfo {
                peer,
                side,
                endpoint,
                source,
                since: SystemTime::now(),
                mine: Vec::new(),
            },
            opener.clone(),
        );
        serve_with(
            &sealed,
            side,
            options,
            None,
            &peer.to_string(),
            &host,
            |mux| opens.run(mux, settings),
        );
        opener.close();
        sealed.close();
        Ok(())
    }

    /// The oldest link that is up to `peer`, to open streams on whichever side made it.
    pub fn peer(&self, peer: &PeerId) -> Option<PeerLink> {
        self.registry.find(peer)
    }

    /// Every link that is up, oldest first.
    pub fn links(&self) -> Vec<LinkInfo> {
        self.registry
            .list_links(self.grants.as_ref())
            .into_iter()
            .map(|(info, _)| info)
            .collect()
    }
}

/// The address a network endpoint names, when it names one.
fn endpoint_source(endpoint: &Endpoint) -> Option<SocketAddr> {
    match endpoint {
        Endpoint::Tcp(address) => Some(*address),
        _ => None,
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
    // A write is the bulk stream this side receives: it is widened to `bulk_window` as soon as it is accepted. The
    // link caps the grant at its `max_window`; a stream that already ended says so to the engine.
    if accepted.protocol == protocol::WRITE && shared.settings.bulk_window > shared.settings.window
    {
        let _ = stream.grant(shared.settings.bulk_window - shared.settings.window);
    }
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

/// How long to wait after `failures` failures in a row: `retry_min` times `retry_factor` that many times, up to
/// `retry_max`, then moved by `retry_jitter` of itself in the direction `random` (from -1 to 1) says.
fn backoff(dial: &DialConfig, failures: u32, random: f64) -> Duration {
    let min = dial.retry_min.0.as_secs_f64();
    let max = dial.retry_max.0.as_secs_f64();
    let grown = min
        * dial
            .retry_factor
            .powi(i32::try_from(failures).unwrap_or(i32::MAX));
    let base = if grown.is_finite() {
        grown.min(max)
    } else {
        max
    };
    let moved = base * (1.0 + dial.retry_jitter * random);
    Duration::from_secs_f64(moved.max(0.001))
}

/// Numbers from -1 to 1 for jitter, seeded by the process's own hasher randomness: nothing depends on them
/// being unpredictable, only on different nodes not retrying in step.
struct Jitter(u64);

impl Jitter {
    fn new() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let seed = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        Self(seed | 1)
    }

    fn next(&mut self) -> f64 {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        #[allow(
            clippy::cast_precision_loss,
            reason = "a fraction of 2^53 steps is all jitter needs"
        )]
        let unit = (self.0 >> 11) as f64 / (1u64 << 53) as f64;
        unit.mul_add(2.0, -1.0)
    }
}

impl Node {
    /// What the local endpoint serves from: the links, the grants and the link settings.
    pub(crate) fn local_parts(&self) -> (Arc<Registry>, Arc<dyn Grants>, Settings) {
        (
            Arc::clone(&self.registry),
            Arc::clone(&self.grants),
            self.shared.settings,
        )
    }
}
