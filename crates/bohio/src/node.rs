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

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use bohio_client::Open as OpenStreams;
use bohio_client::connect::Opener;
pub use bohio_client::dial::Endpoint;
use bohio_client::dial::{Mtu, SerialOptions, ServerTrust, serial};
use bohio_client::link::Settings;
use bohio_client::relay::relay;
use bohio_proto::info::Info;
use bohio_proto::{ErrorBody, ErrorCode, protocol};
use cabuya::auth::{Identity, Proof, Sealed, Session, Trust, prove_out};
use cabuya::link::{Accepted as Open, Host, Options, Services, serve_with};
pub use cabuya::mux::Side;
use cabuya::mux::{CloseReason, LinkStream};
use coqui_peer::{Established, Handshake, KeyMaterial, Method as ProofMethod, PeerId};
use coqui_transport::Channel;

use crate::access::{Capability, DesktopOf, Sources};
use crate::config::ConfigError;
pub use crate::config::{DialConfig, NodeConfig};
use crate::gui::{self, Desktop, GuiState, Notifier, Presence};
use crate::limit::{Counted, Hold, Slots};
use crate::link::{Bridge, NodePeers, PROTOCOLS, admission, grant_of, offered};
pub use crate::link::{
    Grants, GrantsError, Keys, KeysError, LogEvents, MemoryGrants, MemoryKeys, NodeEvent,
    NodeEvents,
};
use crate::listen::Listening;
use crate::pair::PairState;
use crate::pair::{Answered, answer};
pub use crate::pair::{Armed, PairEnd};
use crate::policy::Grant;
use crate::{control, exec, forward, fs, info, media, power, session, shell};

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
    /// A PIN could not be armed.
    #[error("pairing cannot be armed: {0}")]
    Pairing(String),
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
    processes: Arc<Slots>,
    transfers: Slots,
    follows: Slots,
    hashes: Slots,
    walks: Slots,
    searches: Slots,
    sessions: Arc<Slots>,
    power: Arc<Slots>,
    media: Arc<Slots>,
    detached: Arc<session::Sessions>,
    forwards: Slots,
    forwards_per_peer: Counted<String>,
    /// How many streams each peer has open over all its links.
    streams_per_peer: Counted<String>,
    listeners: forward::Listeners,
    events: Arc<EventCell>,
    gui: Arc<GuiState>,
    sources: Arc<Sources>,
}

impl Shared {
    fn new(
        config: &NodeConfig,
        grants: &Arc<dyn Grants>,
        host: PeerId,
        path: OsString,
        events: &Arc<EventCell>,
        gui: &Arc<GuiState>,
        keys: &Arc<dyn Keys>,
    ) -> Arc<Self> {
        let sources = &gui.sources();
        // `[access] fs` and `exec` say whether an engine over the operating system is served at all.
        let mut config = config.clone();
        if sources.names(Capability::Files).is_empty() {
            config.filesystem.enabled = false;
        }
        if sources.names(Capability::Exec).is_empty() {
            config.execution.enabled = false;
        }
        let config = &config;
        if config.compression.bundle_context == crate::config::BundleContext::Shared {
            tracing::warn!(
                "non-default: [compression] bundle_context is shared: the files of a bundle share one compression context, so a path observer can learn from compressed sizes whether a file repeats text placed in the one before it"
            );
        }
        if config.filesystem.allow_id_bits {
            tracing::warn!(
                "non-default: [filesystem] allow_id_bits is true: a peer with fs.write can make a file that runs as this host's account or group for whoever starts it"
            );
        }
        if !config.execution.reset_signals {
            tracing::warn!(
                "non-default: [execution] reset_signals is false: programs keep the signal dispositions this host inherited, so an `interrupt` or `quit` does nothing to one that ignores them"
            );
        }
        if config.execution.enabled {
            use crate::config::ReapOrphans;
            let is_init = std::process::id() == 1;
            match config.execution.reap_orphans {
                ReapOrphans::Always => {
                    crate::platform::collect_adopted_guardians(config.execution.reap_interval.0);
                }
                ReapOrphans::Auto if is_init => {
                    crate::platform::collect_adopted_guardians(config.execution.reap_interval.0);
                }
                ReapOrphans::Never if is_init => tracing::warn!(
                    "non-default: [execution] reap_orphans is never and this host is process 1: each program that ends leaves its guardian as a zombie of this host"
                ),
                _ => {}
            }
        }
        if let Some(dir) = &config.compression.prepare_dir {
            crate::fs::sweep_prepared(dir, config.compression.prepare_ttl.0);
        }
        let ttl = config.filesystem.resume_ttl.0;
        if config.filesystem.enabled && !ttl.is_zero() {
            // What interrupted resumable writes left, in every root a peer may write to: once, off the start's path.
            // Through the default files source, whose directories the roots are named in.
            if let Ok(source) = sources.files(None) {
                let (roots, source) = (grants.roots(), source.arc());
                let limit = config.filesystem.max_tree_entries;
                std::thread::spawn(move || {
                    crate::fs::sweep_resumable(&source, &roots, ttl, limit);
                });
            }
        }
        let mut capabilities = info::collect(config, &path);
        capabilities.host_id = Some(host.to_string());
        Arc::new(Self {
            settings: config.link.settings(),
            grants: Arc::clone(grants),
            path,
            info: capabilities,
            processes: Arc::new(Slots::with_share(
                config.execution.max_processes,
                config.execution.max_processes_per_peer,
            )),
            transfers: Slots::with_share(
                config.filesystem.max_transfers,
                config.filesystem.max_transfers_per_peer,
            ),
            follows: Slots::new(config.filesystem.max_follows),
            hashes: Slots::new(config.hash.max_streams),
            walks: Slots::new(config.walk.max_streams),
            searches: Slots::new(config.search.max_searches),
            sessions: Arc::new(Slots::with_share(
                config.shell.max_sessions,
                config.shell.max_sessions_per_peer,
            )),
            power: Arc::new(Slots::new(config.power.max_streams)),
            media: Arc::new(Slots::new(config.media.max_streams)),
            detached: session::Sessions::new(
                &config.sessions,
                Arc::clone(grants),
                Arc::clone(keys),
                Arc::clone(events) as Arc<dyn NodeEvents>,
            ),
            forwards: Slots::new(config.forward.max_connections),
            forwards_per_peer: Counted::new(config.forward.max_connections_per_peer),
            streams_per_peer: Counted::new(config.link.max_streams_per_peer),
            listeners: forward::Listeners::new(&config.forward),
            events: Arc::clone(events),
            gui: Arc::clone(gui),
            sources: Arc::clone(sources),
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
    ) -> Result<Box<dyn LinkStream>, bohio_client::Error> {
        self.opener.open(protocol, metadata)
    }

    fn open_with(
        &self,
        protocol: &str,
        metadata: &[u8],
        options: &bohio_client::OpenOptions,
    ) -> Result<Box<dyn LinkStream>, bohio_client::Error> {
        self.opener.open_with(protocol, metadata, options)
    }

    fn open_cause(
        &self,
        protocol: &str,
        metadata: &[u8],
    ) -> Result<(Box<dyn LinkStream>, bohio_client::connect::Cause), bohio_client::Error> {
        self.opener.open_cause(protocol, metadata)
    }

    fn bulk_window(&self) -> Option<u32> {
        self.opener.bulk_window()
    }

    fn is_up(&self) -> bool {
        self.opener.is_up()
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

/// The node's grants with the names of each capability's default source filled in, so a peer with no `sources` list reaches
/// the defaults alone.
struct Defaulted {
    inner: Arc<dyn Grants>,
    sources: Arc<Sources>,
}

impl Grants for Defaulted {
    fn grant(&self, peer: &PeerId) -> Result<Grant, crate::link::GrantsError> {
        self.inner
            .grant(peer)
            .map(|grant| grant.with_default_sources(self.sources.default_names()))
    }

    fn roots(&self) -> Vec<std::path::PathBuf> {
        self.inner.roots()
    }
}

/// An embeddable host: its identity, the keys and grants its caller supplies and the engines it serves.
pub struct Node {
    shared: Arc<Shared>,
    grants: Arc<dyn Grants>,
    keys: Arc<dyn Keys>,
    events: Arc<EventCell>,
    gui: Arc<GuiState>,
    sources: Arc<Sources>,
    proof: Proof,
    identity: Identity,
    registry: Arc<Registry>,
    peers: NodePeers,
    host: PeerId,
    connections: AtomicUsize,
    pairing: Arc<PairState>,
    /// Connections still proving themselves, by source: `[link] max_unproven_per_source` each.
    unproven: Counted<IpAddr>,
    /// Proven links, by peer: `[link] max_links_per_peer` each.
    links: Counted<String>,
}

/// The source a connection counts against: an IPv4 address (an IPv4-mapped IPv6 address is its IPv4
/// address), or the /64 an IPv6 address lies in, since one holder controls every address of a /64.
pub(crate) fn source_key(source: SocketAddr) -> IpAddr {
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
        let sources = Arc::new(Sources::unconfigured());
        // A source whose settings hold a secret reference waits for `Node::with_secrets`.
        sources.defer_secrets();
        sources.set_secret_limit(config.secrets.max_bytes);
        sources.set_allow_literal(config.secrets.allow_literal);
        if config.secrets.allow_literal {
            tracing::warn!(
                "non-default: [secrets] allow_literal is true: a sensitive setting may hold its secret in the configuration file, readable by whoever can read that file and its backups"
            );
        }
        // Libraries are mapped before the sources are built, so a `[source.<name>]` can name what they offer.
        sources.load_libraries(&config.providers);
        sources
            .reconfigure(&config.source, &config.access)
            .map_err(|(key, message)| {
                NodeError::Config(ConfigError::Invalid {
                    path: "the node settings".to_owned(),
                    key,
                    message,
                })
            })?;
        // Every grant the node reads says which sources are each capability's default now.
        let grants: Arc<dyn Grants> = Arc::new(Defaulted {
            inner: grants,
            sources: Arc::clone(&sources),
        });
        let gui = GuiState::new(&config.gui, Arc::clone(&sources));
        let proof = Proof::builder(identity.clone())
            .initiators(asked)
            .build()
            .map_err(|error| NodeError::Proof(error.to_string()))?;
        Ok(Self {
            identity,
            registry: Arc::default(),
            shared: Shared::new(
                &config,
                &grants,
                host,
                OsString::new(),
                &events,
                &gui,
                &keys,
            ),
            gui,
            sources,
            peers: NodePeers::new(Arc::clone(&keys), Arc::clone(&grants)),
            grants,
            keys,
            events,
            proof,
            host,
            connections: AtomicUsize::new(0),
            pairing: Arc::new(PairState::new(config.pairing.clone())),
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
        self.shared = Shared::new(
            &self.shared.config,
            &self.grants,
            self.host,
            path,
            &self.events,
            &self.gui,
            &self.keys,
        );
        self
    }

    /// Says how the references of sensitive source settings are read (`file:`, `credential:`): the library reads
    /// no file and no environment variable, so the embedder's resolver does. Sources that were waiting for it are built.
    ///
    /// # Errors
    ///
    /// [`NodeError::Config`] naming `source.<name>.<key>` for a reference the resolver refuses; the message never quotes it.
    pub fn with_secrets(
        self,
        resolver: Arc<dyn crate::secret::SecretResolver>,
    ) -> Result<Self, NodeError> {
        self.sources
            .set_secrets(resolver, self.shared.config.secrets.max_bytes)
            .map_err(|(key, message)| {
                NodeError::Config(ConfigError::Invalid {
                    path: "the node settings".to_owned(),
                    key,
                    message,
                })
            })?;
        Ok(self)
    }

    /// The sources this node serves from, to change what they are while it runs ([`Sources::reconfigure`]).
    #[must_use]
    pub fn sources(&self) -> Arc<Sources> {
        Arc::clone(&self.sources)
    }

    /// Serves from `sources` instead of the ones the settings describe. Call it first: it rebuilds what holds them, so
    /// a presence or notifier set before is lost.
    #[must_use]
    pub fn with_sources(mut self, sources: Sources) -> Self {
        self.sources.absorb(sources);
        self.shared = Shared::new(
            &self.shared.config,
            &self.grants,
            self.host,
            self.shared.path.clone(),
            &self.events,
            &self.gui,
            &self.keys,
        );
        self
    }

    /// The desktop this node serves over `bohio-gui/1` (needs `[gui] enabled` and a peer holding `gui.view` or
    /// `gui.input`). A platform backend and an application's own are the same thing: a [`Desktop`].
    #[must_use]
    pub fn with_desktop(self, desktop: Arc<dyn Desktop>) -> Self {
        self.sources
            .add_desktop("app", Arc::new(DesktopOf::new(desktop)));
        self.sources.make_default(Capability::Desktop, "app");
        self
    }

    /// Replaces what tells the person at the screen, and asks them: the node's own [`gui::LocalPresence`] otherwise.
    #[must_use]
    pub fn with_presence(self, presence: Arc<dyn Presence>) -> Self {
        self.gui.set_presence(presence);
        self
    }

    /// Where the node's own presence shows its notices; they are logged until this says otherwise.
    #[must_use]
    pub fn with_notifier(self, notifier: Arc<dyn Notifier>) -> Self {
        self.gui.set_notifier(notifier);
        self
    }

    /// The desktop approvals the node waits on, which `bohio-gui-approve/1` of the local endpoint lists and decides.
    pub fn gui_approvals(&self) -> Arc<gui::Approvals> {
        self.gui.approvals()
    }

    /// Lets this node pair peers on its own listeners, accepting a joiner's key in `store` (the trust store `respond`
    /// needs) and recording it through [`Keys::paired`]. A node without this refuses every joiner unanswered.
    ///
    /// # Errors
    ///
    /// [`NodeError::Proof`] when the pairing desk cannot be made.
    pub fn with_pairing(self, store: coqui_peer::TrustStore) -> Result<Self, NodeError> {
        self.pairing.enable(store)?;
        Ok(self)
    }

    /// Arms a new PIN, lasting `lifetime` (`[pairing] timeout` when `None`), and withdraws the one armed before: a
    /// joiner on any of this node's listeners may try it once.
    ///
    /// # Errors
    ///
    /// [`NodeError::Pairing`] when the node was not given a store by [`Node::with_pairing`] or the lifetime is zero.
    pub fn arm_pairing(&self, lifetime: Option<Duration>) -> Result<Armed, NodeError> {
        self.pairing.arm(lifetime)
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
                let Some(arrival) = listening.accept(settings.accept_wait, settings)? else {
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
            thread::sleep(settings.serial_reopen);
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

    /// Proves the peer on `channel` and serves its link until it ends: at most `[link] max_connections` links at once
    /// across peers, at most `[link] max_unproven_per_source` still proving themselves from one `source` (known only
    /// when the caller passes it), and at most `[link] max_links_per_peer` per peer. A channel past a bound is
    /// refused: this returns at once and logs it. The caller closes `channel`.
    pub fn serve_channel<C: Channel>(&self, channel: &C, source: Option<SocketAddr>) {
        let proving = match source {
            Some(source) => match self.unproven.take(source_key(source)) {
                Some(hold) => Some(hold),
                None => {
                    tracing::info!(
                        "a connection was refused: its source has max_unproven_per_source connections proving themselves"
                    );
                    return;
                }
            },
            None => None,
        };
        if self.connections.fetch_add(1, Ordering::SeqCst)
            >= self.shared.config.link.max_connections
        {
            self.connections.fetch_sub(1, Ordering::SeqCst);
            tracing::info!("a connection was refused: max_connections are open");
            return;
        }
        self.serve_proving(channel, source, proving);
        self.connections.fetch_sub(1, Ordering::SeqCst);
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
        let key = source.map(source_key);
        let mut attempts = 0;
        let mut delayed = false;
        let answered = self.pairing.host(&self.identity, key, |host, slot, wait| {
            delayed = wait;
            let answered = answer(
                channel,
                &sourced,
                settings.record_length(self.proof.method()),
                host,
                source,
                settings.proof_timeout,
                self.shared.config.pairing.exchange_timeout.0,
            );
            if let Some(slot) = slot {
                slot.attempted(matches!(answered, Ok(Answered::Paired(_))));
                if !wait && matches!(&answered, Err(unanswered) if unanswered.pairing) {
                    attempts = slot.failed(key);
                }
            }
            if let Ok(Answered::Paired(paired)) = &answered {
                let end = self.record_pairing(&paired.peer);
                if let Some(slot) = slot {
                    slot.finish(end);
                }
            }
            answered
        });
        drop(proving);
        let established = match answered {
            Ok(Answered::Proven(established)) => established,
            Ok(_) => return,
            Err(unanswered) if unanswered.pairing => {
                let reason = if delayed {
                    "too soon after a wrong PIN from this source".to_owned()
                } else {
                    unanswered.error.to_string()
                };
                tracing::info!(?source, attempts, "a pairing was refused: {reason}");
                return self.events.event(&NodeEvent::PairingRefused {
                    source,
                    attempts,
                    reason,
                });
            }
            Err(unanswered) => match unanswered.error {
                error @ cabuya::Error::Proof(_) => {
                    return tracing::info!(
                        "a key was refused: {error}; a key that proved itself and is not accepted waits for `bohio peer approve`"
                    );
                }
                error => return tracing::info!("a connection ended before its proof: {error}"),
            },
        };
        let _ = self.serve_proven(channel, established, Side::Answered, source, None);
    }

    /// Hands a paired joiner's key to [`Keys::paired`] and tells the events.
    fn record_pairing(&self, key: &KeyMaterial) -> PairEnd {
        let peer = key.id();
        match self.keys.paired(&peer, key) {
            Ok(()) => {
                self.events.event(&NodeEvent::Paired { peer });
                PairEnd::Paired(peer)
            }
            Err(error) => {
                let reason = format!("{peer} paired and could not be recorded: {error}");
                tracing::warn!("{reason}");
                self.events.event(&NodeEvent::PairingRefused {
                    source: None,
                    attempts: 0,
                    reason: reason.clone(),
                });
                PairEnd::Failed(reason)
            }
        }
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
                    cabuya::Error::Proof(_) => {
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
    /// exists, whichever side made it, nor while [`Keys`] do not accept `peer` (asked again every `retry_min`), so a
    /// revoked peer is not reached again. Two links made at once (the peer dialing this node as this node dials it)
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
        let mut jitter = bohio_client::keep::Jitter::new();
        let not_accepted = "the peer's key is not accepted: no dial until it is".to_owned();
        while !stop.load(Ordering::SeqCst) {
            if self.peer(peer).is_some() {
                thread::sleep(poll);
                continue;
            }
            // A dial proves this node to whoever answers, so a peer revoked since the dialer started is not
            // reached again; its keys are asked again every `retry_min`.
            if !self.keys.is_accepted(peer) {
                if told.as_ref() != Some(&not_accepted) {
                    self.events.event(&NodeEvent::DialFailed {
                        peer: *peer,
                        endpoint: endpoint.to_owned(),
                        reason: not_accepted.clone(),
                        retry_in: dial.retry_min.0,
                    });
                    told = Some(not_accepted.clone());
                }
                let until = Instant::now() + dial.retry_min.0;
                while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                    thread::sleep(poll.min(until.saturating_duration_since(Instant::now())));
                }
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
                .and_then(|parsed| match dial.web_mtu() {
                    Ok(mtu) if mtu != Mtu::default() => parsed.with_mtu(mtu),
                    Ok(_) => Ok(parsed),
                    Err((key, reason)) => Err(format!("{key} {reason}")),
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
            let wait = dial.backoff().wait(failures, jitter.sample());
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

    /// Keeps the forwards `declarations` lists (`[peer."<id>".forward] local` and `remote`) up until `stop` is set: each local
    /// entry's listener is bound once and kept, a connection that arrives while there is no link to the peer waits up to `[forward]
    /// wait_for_link` for one, and each remote entry is asked of the peer's host again, for the same port, whenever a link to
    /// the peer comes up. `declarations` is read again every `[link] recheck`: an entry added is started and an entry removed
    /// stops, as grants follow the file. Progress is told as [`NodeEvent::Kept`]. The links are the ones this node holds, dialed
    /// by [`Node::run_dial`] or by the peer.
    pub fn run_forwards(&self, declarations: &dyn forward::Declarations, stop: &AtomicBool) {
        let registry = Arc::clone(&self.registry);
        let links_for = move |peer: PeerId| -> Arc<dyn bohio_client::keep::Links> {
            let registry = Arc::clone(&registry);
            Arc::new(move || {
                registry
                    .find(&peer)
                    .map(|link| Arc::new(link) as Arc<dyn OpenStreams>)
            })
        };
        let keeper = forward::Keeper {
            links_for: &links_for,
            settings: self.shared.config.forward.client(),
            recheck: self.shared.config.link.recheck.0,
            poll: self.shared.settings.poll_interval,
            events: &*self.events,
        };
        forward::keep_forwards(&keeper, declarations, stop);
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
        // A revoked key's streams end at the next re-check and hear why; the link itself ends at its own key check.
        let sealed = Sealed::new(channel, established.session)
            .with_check(move || keys.is_accepted(&peer))
            .with_check_every(
                self.shared
                    .config
                    .link
                    .key_recheck
                    .map_or_else(|| recheck.saturating_mul(2), |every| every.0),
            );
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
    // A peer's streams are counted over all its links, so many links do not multiply what one peer may hold.
    let Some(_held) = shared.streams_per_peer.take(accepted.peer.to_string()) else {
        return stream.close_with(
            CloseReason::Refused,
            "this peer has as many streams open as the host allows",
        );
    };
    // A write is the bulk stream this side receives: it is widened to `bulk_window` as soon as it is accepted. The
    // link caps the grant at its `max_window`; a stream that already ended says so to the engine.
    if accepted.protocol == protocol::WRITE && shared.settings.bulk_window > shared.settings.window
    {
        let _ = stream.grant(shared.settings.bulk_window - shared.settings.window);
    }
    if accepted.protocol == protocol::GUI {
        let Ok(peer) = accepted.peer.parse::<PeerId>() else {
            return stream.close_with(CloseReason::Refused, "that is not a peer id");
        };
        let now = || {
            grant_of(shared.grants.as_ref(), &accepted.peer).map_err(|_| {
                ErrorBody::new(ErrorCode::Internal, "the host cannot read its peer grants")
            })
        };
        let context = gui::Context {
            grant: &grant,
            current: Some(control::Current(&now)),
            config: &shared.config.gui,
            state: &shared.gui,
            peer,
            events: shared.events.as_ref(),
            session: gui::DEFAULT_SESSION,
        };
        thread::scope(|scope| {
            let relay = relay(scope, stream, &shared.settings);
            let channel: Arc<dyn Channel> = relay.clone();
            gui::serve_chosen(&channel, &accepted.metadata, &context);
            relay.close();
        });
        return;
    }
    if matches!(
        accepted.protocol.as_str(),
        protocol::POWER | protocol::MEDIA
    ) {
        let Ok(peer) = accepted.peer.parse::<PeerId>() else {
            return stream.close_with(CloseReason::Refused, "that is not a peer id");
        };
        thread::scope(|scope| {
            let relay = relay(scope, stream, &shared.settings);
            let channel: Arc<dyn Channel> = relay.clone();
            if accepted.protocol == protocol::POWER {
                let context = power::Context {
                    grant: &grant,
                    config: &shared.config.power,
                    slots: &shared.power,
                    sources: &shared.sources,
                    peer,
                    events: shared.events.as_ref(),
                };
                let _ = power::serve(&channel, &accepted.metadata, &context);
            } else {
                let context = media::Context {
                    grant: &grant,
                    config: &shared.config.media,
                    slots: &shared.media,
                    sources: &shared.sources,
                    peer,
                    events: shared.events.as_ref(),
                };
                let _ = media::serve(&channel, &accepted.metadata, &context);
            }
            relay.close();
        });
        return;
    }
    // A forward carries a connection's bytes on the stream itself, with no relay in between.
    if matches!(
        accepted.protocol.as_str(),
        protocol::CONNECT | protocol::TCP | protocol::LISTEN | protocol::ACCEPT
    ) {
        let Ok(peer) = accepted.peer.parse::<PeerId>() else {
            return stream.close_with(CloseReason::Refused, "that is not a peer id");
        };
        let context = forward::Context {
            grant: &grant,
            config: &shared.config.forward,
            slots: &shared.forwards,
            per_peer: &shared.forwards_per_peer,
            listeners: &shared.listeners,
            peer,
            events: shared.events.as_ref(),
        };
        match accepted.protocol.as_str() {
            protocol::CONNECT => forward::serve_connect(stream, &accepted.metadata, &context),
            protocol::LISTEN => forward::serve_listen(stream, &accepted.metadata, &context),
            protocol::ACCEPT => forward::serve_accept(stream, &accepted.metadata, &context),
            _ => forward::serve_tcp(stream, &accepted.metadata, &context),
        }
        return;
    }
    let peer = accepted.peer.to_string();
    thread::scope(|scope| {
        let relay = relay(scope, stream, &shared.settings);
        let channel: Arc<dyn Channel> = relay.clone();
        let metadata = accepted.metadata.as_slice();
        let config = &shared.config;
        // Each item of a bundle, root of a walk or hash is decided by the grant as it is then, not as it was
        // when the stream opened.
        let now = || {
            grant_of(shared.grants.as_ref(), &accepted.peer).map_err(|_| {
                ErrorBody::new(ErrorCode::Internal, "the host cannot read its peer grants")
            })
        };
        let picked = fs::pick(
            &shared.sources,
            &grant,
            fs::requested_source(accepted.protocol.as_str(), metadata).as_deref(),
        );
        let files = fs::Context {
            source: picked.by_ref(),
            grant: &grant,
            config: &config.filesystem,
            hash: &config.hash,
            delta: &config.delta,
            compression: &config.compression,
            peer: &peer,
            slots: &shared.transfers,
            follows: &shared.follows,
            current: Some(control::Current(&now)),
        };
        match accepted.protocol.as_str() {
            protocol::EXEC if session::wants_exec(metadata) => {
                let host = session::ExecHost {
                    grant: &grant,
                    config: &config.execution,
                    compression: &config.compression,
                    slots: &shared.processes,
                    path: &shared.path,
                    sources: &shared.sources,
                };
                match shared.detached.serve_exec(&channel, metadata, &peer, &host) {
                    Ok(ended) => finish_attached(&relay, ended),
                    Err(error) => tracing::info!(code = ?error.code, "session refused"),
                }
            }
            protocol::SHELL if session::wants_console(metadata) => {
                let host = session::ShellHost {
                    grant: &grant,
                    config: &config.shell,
                    slots: &shared.sessions,
                    path: &shared.path,
                    sources: &shared.sources,
                };
                match shared
                    .detached
                    .serve_console(&channel, metadata, &peer, &host)
                {
                    Ok(ended) => finish_attached(&relay, ended),
                    Err(error) => tracing::info!(code = ?error.code, "console refused"),
                }
            }
            protocol::SHELL if session::wants_shell(metadata) => {
                let host = session::ShellHost {
                    grant: &grant,
                    config: &config.shell,
                    slots: &shared.sessions,
                    path: &shared.path,
                    sources: &shared.sources,
                };
                match shared
                    .detached
                    .serve_shell(&channel, metadata, &peer, &host)
                {
                    Ok(ended) => finish_attached(&relay, ended),
                    Err(error) => tracing::info!(code = ?error.code, "session refused"),
                }
            }
            protocol::ATTACH => {
                match shared
                    .detached
                    .serve_attach(&channel, metadata, &peer, &grant)
                {
                    Ok(ended) => finish_attached(&relay, ended),
                    Err(error) => tracing::info!(code = ?error.code, "attach refused"),
                }
            }
            protocol::EXEC => {
                let context = exec::Context {
                    grant: &grant,
                    config: &config.execution,
                    compression: &config.compression,
                    slots: &shared.processes,
                    peer: &peer,
                    path: &shared.path,
                    sources: &shared.sources,
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
                    peer: &peer,
                    path: &shared.path,
                    sources: &shared.sources,
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
            protocol::WALK => {
                let context = fs::WalkContext {
                    files,
                    walk: &config.walk,
                    slots: &shared.walks,
                };
                match fs::serve_walk(channel, metadata, &context) {
                    Ok(entries) => tracing::debug!(entries, "walk ended"),
                    Err(error) => tracing::info!(code = ?error.code, "walk refused"),
                }
            }
            protocol::SEARCH => {
                let context = fs::SearchContext {
                    files,
                    search: &config.search,
                    slots: &shared.searches,
                };
                match fs::serve_search(channel, metadata, &context) {
                    Ok(matches) => tracing::debug!(matches, "search ended"),
                    Err(error) => tracing::info!(code = ?error.code, "search refused"),
                }
            }
            protocol::HASH => {
                let context = fs::HashContext {
                    files,
                    slots: &shared.hashes,
                    current: Some(control::Current(&now)),
                };
                match fs::serve_hash(channel, metadata, &context) {
                    Ok(items) => tracing::debug!(items, "hash ended"),
                    Err(error) => tracing::info!(code = ?error.code, "hash refused"),
                }
            }
            protocol::CONTROL => {
                let probe = |grant: &Grant| shared.gui.info(&config.gui, grant);
                let access = |grant: &Grant| shared.sources.info(grant);
                let named =
                    |grant: &Grant, name: Option<&str>| fs::pick(&shared.sources, grant, name);
                let context = control::Context {
                    filesystem: files,
                    info: &shared.info,
                    current: Some(control::Current(&now)),
                    gui: Some(control::GuiProbe(&probe)),
                    path: Some(shared.path.as_os_str()),
                    access: Some(control::AccessProbe(&access)),
                    files: Some(control::FilesProbe(&named)),
                    sessions: Some(control::SessionsView {
                        registry: &shared.detached,
                        peer: &peer,
                    }),
                };
                let answered = control::serve(&channel, &context);
                tracing::debug!(answered, "control ended");
            }
            _ => {}
        }
        relay.close();
    });
}

/// Closes an attached stream as the way it ended says: another attach took its place, or it is done.
fn finish_attached(relay: &bohio_client::relay::Relay, ended: session::Ended) {
    if ended == session::Ended::Superseded {
        relay.close_with(
            CloseReason::Superseded,
            "another stream attached to the session",
        );
    }
}

impl Node {
    /// The node's armed pairing, for the local endpoint's `bohio-pair/1`.
    pub(crate) fn pair_state(&self) -> Arc<PairState> {
        Arc::clone(&self.pairing)
    }

    /// What the local endpoint serves from: the links, the grants and the link settings.
    /// What the helper endpoint needs: the desktop state, the link settings and where events go.
    pub(crate) fn gui_parts(&self) -> GuiParts {
        GuiParts {
            state: Arc::clone(&self.gui),
            settings: self.shared.settings,
            events: Arc::clone(&self.events) as Arc<dyn NodeEvents>,
            host: self.host,
        }
    }

    pub(crate) fn local_parts(&self) -> (Arc<Registry>, Arc<dyn Grants>, Settings) {
        (
            Arc::clone(&self.registry),
            Arc::clone(&self.grants),
            self.shared.settings,
        )
    }
}

/// What [`Node::gui_parts`] hands the helper endpoint.
pub(crate) struct GuiParts {
    pub(crate) state: Arc<GuiState>,
    pub(crate) settings: Settings,
    pub(crate) events: Arc<dyn NodeEvents>,
    pub(crate) host: PeerId,
}
