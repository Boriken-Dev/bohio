//! Who may open which protocol, and what a node tells its owner: the traits a node asks, the provided
//! in-memory implementations and the events.
//!
//! [`Keys`] says who a peer is (an accepted key); [`Grants`] says what it may do. A node asks them
//! at every decision, so a change by their owner applies at the next open or [`recheck`] without a
//! restart: [`admission`] decides each stream when it opens and every `[link] recheck` while it
//! runs, and the link itself checks the key every `[link] key_recheck`.
//!
//! [`recheck`]: crate::config::LinkConfig::recheck

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use coqui_link::Error;
use coqui_link::link::Admission;
use coqui_link::mux::Side;
use coqui_link::peers::{Event, Events, PeerRecord, PeerStore};
use coqui_peer::{KeyMaterial, PeerId};

use bohio_proto::protocol;

use crate::policy::{Grant, Permission};

/// The protocols a host serves, each with the permissions of which a peer needs at least one to open it.
pub const PROTOCOLS: [(&str, &[Permission]); 11] = [
    (
        protocol::CONTROL,
        &[Permission::Info, Permission::FsRead, Permission::FsWrite],
    ),
    (protocol::EXEC, &[Permission::Exec]),
    (protocol::SHELL, &[Permission::Shell]),
    (protocol::READ, &[Permission::FsRead]),
    (protocol::WRITE, &[Permission::FsWrite]),
    (protocol::HASH, &[Permission::FsRead]),
    (protocol::CONNECT, &[Permission::ForwardConnect]),
    (protocol::TCP, &[Permission::ForwardConnect]),
    (protocol::GUI, &[Permission::GuiView, Permission::GuiInput]),
    (protocol::LISTEN, &[Permission::ForwardListen]),
    (protocol::ACCEPT, &[Permission::ForwardListen]),
];

/// Which keys a node accepts.
///
/// It is asked when a peer's proof holds (a key it does not accept ends the connection), at every
/// stream decision and by the link every `[link] key_recheck`, so an implementation answers from
/// where its owner keeps the list, and answers quickly.
pub trait Keys: Send + Sync {
    /// Whether `peer` is accepted now. A question that cannot be answered is `false`.
    fn is_accepted(&self, peer: &PeerId) -> bool;

    /// Hears a key that proved itself and was refused, on either side of a link. `source` is where the
    /// connection came from, when the node knows. The key is proven, so it is safe to queue for an
    /// operator; the default ignores it.
    fn stranger(&self, _peer: &PeerId, _key: &KeyMaterial, _source: Option<SocketAddr>) {}

    /// Records `peer`, whose key just paired by the node's armed PIN, as accepted, holding nothing: it is accepted
    /// from the next question on. Where that goes (a table in a file, a database row) is the implementation's.
    ///
    /// # Errors
    ///
    /// [`KeysError`] when it cannot be recorded; the default cannot record anything, so a node whose keys do not
    /// override it pairs nobody.
    fn paired(&self, _peer: &PeerId, _key: &KeyMaterial) -> Result<(), KeysError> {
        Err(KeysError::new("these keys cannot record a pairing"))
    }
}

/// Why a paired key could not be recorded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct KeysError(String);

impl KeysError {
    /// An error with `reason`, which holds no key, PIN or request.
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// Why a peer's grant could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct GrantsError(String);

impl GrantsError {
    /// An error with `reason`, which holds no key, PIN or request.
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// What each accepted peer may do.
///
/// It is asked at every stream's open, at every `[link] recheck` of a running stream and at every
/// `bohio/1` call, so a narrowed grant ends what it no longer covers. An error refuses.
pub trait Grants: Send + Sync {
    /// The grant of `peer` as it is now: [`Grant::none`] for a peer the owner lists nothing for.
    ///
    /// # Errors
    ///
    /// [`GrantsError`] when the owner's source cannot be read; the node then refuses.
    fn grant(&self, peer: &PeerId) -> Result<Grant, GrantsError>;
}

/// The accepted keys, in memory: [`MemoryKeys::accept`] and [`MemoryKeys::remove`] apply at the next
/// decision. A stranger is ignored.
#[derive(Debug, Default)]
pub struct MemoryKeys {
    accepted: RwLock<BTreeSet<PeerId>>,
}

impl MemoryKeys {
    /// No key accepted.
    pub fn new() -> Self {
        Self::default()
    }

    /// Accepts `peer`.
    pub fn accept(&self, peer: PeerId) {
        self.accepted
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(peer);
    }

    /// Stops accepting `peer`; `true` when it was accepted.
    pub fn remove(&self, peer: &PeerId) -> bool {
        self.accepted
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(peer)
    }
}

impl FromIterator<PeerId> for MemoryKeys {
    fn from_iter<I: IntoIterator<Item = PeerId>>(peers: I) -> Self {
        Self {
            accepted: RwLock::new(peers.into_iter().collect()),
        }
    }
}

impl Keys for MemoryKeys {
    fn is_accepted(&self, peer: &PeerId) -> bool {
        self.accepted
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(peer)
    }

    fn paired(&self, peer: &PeerId, _key: &KeyMaterial) -> Result<(), KeysError> {
        self.accept(*peer);
        Ok(())
    }
}

/// The grants, in memory: [`MemoryGrants::set`] and [`MemoryGrants::remove`] apply at the next decision,
/// and a peer with no entry holds nothing. Collected from pairs it is a fixed map.
#[derive(Debug, Default)]
pub struct MemoryGrants {
    grants: RwLock<BTreeMap<PeerId, Grant>>,
}

impl MemoryGrants {
    /// No peer granted anything.
    pub fn new() -> Self {
        Self::default()
    }

    /// Gives `peer` exactly `grant`, replacing any it had.
    pub fn set(&self, peer: PeerId, grant: Grant) {
        self.grants
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(peer, grant);
    }

    /// Takes away `peer`'s grant; `true` when it had one.
    pub fn remove(&self, peer: &PeerId) -> bool {
        self.grants
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(peer)
            .is_some()
    }
}

impl FromIterator<(PeerId, Grant)> for MemoryGrants {
    fn from_iter<I: IntoIterator<Item = (PeerId, Grant)>>(grants: I) -> Self {
        Self {
            grants: RwLock::new(grants.into_iter().collect()),
        }
    }
}

impl Grants for MemoryGrants {
    fn grant(&self, peer: &PeerId) -> Result<Grant, GrantsError> {
        Ok(self
            .grants
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(peer)
            .cloned()
            .unwrap_or_default())
    }
}

/// Something that happened on a node's links: an id, a protocol name or an address, never a key, a PIN
/// or a request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NodeEvent {
    /// A peer proved itself and its link came up; `side` says whether this node answered or dialed.
    LinkUp {
        /// The peer.
        peer: PeerId,
        /// This node's side of the link.
        side: Side,
    },
    /// A link ended.
    LinkDown {
        /// The peer.
        peer: PeerId,
    },
    /// A stream was opened and served.
    Opened {
        /// The peer.
        peer: PeerId,
        /// The protocol.
        protocol: String,
    },
    /// A stream was refused.
    Refused {
        /// The peer.
        peer: PeerId,
        /// The protocol.
        protocol: String,
    },
    /// A running stream ended because the peer lost its grant.
    Revoked {
        /// The peer.
        peer: PeerId,
        /// The protocol.
        protocol: String,
    },
    /// This node is dialing a peer's endpoint (an attempt: one is made again after each wait).
    Dialing {
        /// The peer the dial expects.
        peer: PeerId,
        /// The endpoint, as text.
        endpoint: String,
    },
    /// A dial did not give a link, or the link it gave ended at once. Told once per change of reason, not at every
    /// retry of the same one.
    DialFailed {
        /// The peer the dial expects.
        peer: PeerId,
        /// The endpoint, as text.
        endpoint: String,
        /// Why, with no key, PIN or request in it.
        reason: String,
        /// How long until the next attempt.
        retry_in: Duration,
    },
    /// A peer paired by the armed PIN and [`Keys::paired`] recorded it; it holds nothing until granted.
    Paired {
        /// The peer.
        peer: PeerId,
    },
    /// A joiner was refused or failed: no PIN was armed, it was wrong or expired, or the pairing broke. The reason holds
    /// no key or PIN.
    PairingRefused {
        /// Where it connected from, when known.
        source: Option<SocketAddr>,
        /// Wrong or failed attempts against the armed PIN so far, this one included; `0` when none is armed or the source
        /// was refused for trying too soon.
        attempts: u32,
        /// Why.
        reason: String,
    },
    /// A key proved itself and [`Keys`] did not accept it.
    Stranger {
        /// The key's id.
        peer: PeerId,
        /// Where it connected from, when known.
        source: Option<SocketAddr>,
    },
    /// The host made a connection for a peer, or could not. The target is its text, never a byte of what it carried.
    Forwarded {
        /// The peer.
        peer: PeerId,
        /// What kind of forward.
        kind: ForwardKind,
        /// The target or bind, as `HOST:PORT`, `unix:PATH` or `pipe:NAME`.
        target: String,
        /// How it went.
        outcome: ForwardOutcome,
    },
    /// A forward this node keeps toward a peer (`[peer."<id>".forward] local` or `remote`) changed state.
    Kept {
        /// The peer.
        peer: PeerId,
        /// Whether the entry is a `local` or a `remote` one.
        direction: KeptDirection,
        /// The entry as written in the file.
        spec: String,
        /// What it is now.
        state: KeptState,
    },
    /// Something happened on a peer's view of the desktop. Counts and ids only: never pixels, typed text, key names or
    /// clipboard content.
    Gui {
        /// The peer.
        peer: PeerId,
        /// The desktop session.
        session: String,
        /// What happened.
        change: GuiChange,
    },
}

/// Which list of `[peer."<id>".forward]` a [`NodeEvent::Kept`] entry is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KeptDirection {
    /// `local`: this node listens and the peer's host connects.
    Local,
    /// `remote`: the peer's host listens and this node connects.
    Remote,
}

/// What a forward this node keeps is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeptState {
    /// It listens (a local entry, bound for as long as the entry stays) or the host listens for it (a remote entry, asked
    /// again after each loss): `address` is where, with the port the system or the host chose.
    Up {
        /// The bound address, as `HOST:PORT`, `unix:PATH` or `pipe:NAME`.
        address: String,
    },
    /// It is not listening: its link ended (a remote entry), or the entry was removed or the node stopped.
    Down,
    /// It could not listen: the reason holds no key or request. Told once per change of reason.
    Failed {
        /// Why.
        reason: String,
        /// How long until it is tried again.
        retry_in: Duration,
    },
}

/// What a [`NodeEvent::Gui`] tells of.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GuiChange {
    /// A stream opened, with what the peer was granted of what it asked.
    Started {
        /// What the stream may do.
        granted: Vec<bohio_proto::gui::Want>,
    },
    /// A stream ended, with what it did.
    Ended {
        /// Snapshots sent.
        snapshots: u64,
        /// Screen updates sent.
        frames: u64,
        /// Input events applied.
        input_events: u64,
        /// Clipboard reads.
        clipboard_reads: u64,
        /// Clipboard writes.
        clipboard_writes: u64,
        /// Why it ended.
        reason: String,
    },
    /// The clipboard was read.
    ClipboardRead,
    /// The clipboard was written.
    ClipboardWritten,
    /// An open was refused.
    Refused {
        /// Why, with no request content in it.
        reason: String,
    },
}

/// What kind of forward a [`NodeEvent::Forwarded`] tells of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ForwardKind {
    /// The host connected to a target for the peer.
    Connect,
    /// The host bound a listener for the peer (the event's target is what was bound).
    Listen,
    /// The peer claimed a connection one of its listeners took (the event's target is the listener's address).
    Accept,
}

/// How a forward began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ForwardOutcome {
    /// The connection was made and its bytes are carried.
    Connected,
    /// Nothing was connected, for this reason.
    Failed(bohio_proto::ErrorCode),
}

/// Where a node's events go.
pub trait NodeEvents: Send + Sync {
    /// Hears one event. It runs on the link's own thread: it returns quickly and never blocks on the node.
    fn event(&self, event: &NodeEvent);
}

/// Events as log lines: the default of a node.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogEvents;

impl NodeEvents for LogEvents {
    fn event(&self, event: &NodeEvent) {
        match event {
            NodeEvent::LinkUp { peer, side } => tracing::info!(%peer, ?side, "link up"),
            NodeEvent::LinkDown { peer } => tracing::info!(%peer, "link down"),
            NodeEvent::Opened { peer, protocol } => {
                tracing::debug!(%peer, %protocol, "stream opened");
            }
            NodeEvent::Refused { peer, protocol } => {
                tracing::info!(%peer, %protocol, "stream refused");
            }
            NodeEvent::Revoked { peer, protocol } => {
                tracing::info!(%peer, %protocol, "stream ended: the peer lost its grant");
            }
            NodeEvent::Dialing { peer, endpoint } => tracing::info!(%peer, %endpoint, "dialing"),
            NodeEvent::DialFailed {
                peer,
                endpoint,
                reason,
                retry_in,
            } => tracing::info!(%peer, %endpoint, %reason, ?retry_in, "dial failed"),
            NodeEvent::Paired { peer } => tracing::info!(%peer, "paired"),
            NodeEvent::PairingRefused {
                source,
                attempts,
                reason,
            } => {
                tracing::info!(?source, attempts, %reason, "a pairing was refused");
            }
            NodeEvent::Stranger { peer, source } => {
                tracing::info!(%peer, ?source, "a key proved itself and is not accepted");
            }
            NodeEvent::Forwarded {
                peer,
                kind,
                target,
                outcome,
            } => tracing::info!(%peer, ?kind, %target, ?outcome, "forward"),
            NodeEvent::Kept {
                peer,
                direction,
                spec,
                state,
            } => tracing::info!(%peer, ?direction, %spec, ?state, "kept forward"),
            NodeEvent::Gui {
                peer,
                session,
                change,
            } => tracing::info!(%peer, %session, ?change, "gui"),
        }
    }
}

/// Link events, as a node's events: the link names a peer by its text.
pub(crate) struct Bridge<'a> {
    pub(crate) events: &'a dyn NodeEvents,
    pub(crate) side: Side,
}

impl Events for Bridge<'_> {
    fn event(&self, event: &Event) {
        let named = |peer: &str, make: &dyn Fn(PeerId) -> NodeEvent| {
            if let Ok(peer) = peer.parse::<PeerId>() {
                self.events.event(&make(peer));
            }
        };
        match event {
            Event::LinkUp { peer } => named(peer, &|peer| NodeEvent::LinkUp {
                peer,
                side: self.side,
            }),
            Event::LinkDown { peer } => named(peer, &|peer| NodeEvent::LinkDown { peer }),
            Event::Opened { peer, protocol } => named(peer, &|peer| NodeEvent::Opened {
                peer,
                protocol: protocol.clone(),
            }),
            Event::Refused { peer, protocol } => named(peer, &|peer| NodeEvent::Refused {
                peer,
                protocol: protocol.clone(),
            }),
            Event::Revoked { peer, protocol } => named(peer, &|peer| NodeEvent::Revoked {
                peer,
                protocol: protocol.clone(),
            }),
            _ => {}
        }
    }
}

/// The accepted keys as a link's peer store: an accepted key is a peer, and it may open each protocol the
/// host serves for which its [`Grants`] entry holds a permission ([`PROTOCOLS`]). That is what the link's
/// `link/1` document offers the peer, so the document says what this node would serve it now; [`admission`]
/// still decides every open and every re-check, narrowing further by what the open asks for.
#[derive(Clone)]
pub struct NodePeers {
    keys: Arc<dyn Keys>,
    grants: Arc<dyn Grants>,
}

impl core::fmt::Debug for NodePeers {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NodePeers").finish_non_exhaustive()
    }
}

impl NodePeers {
    /// The peers `keys` accepts, each offered what `grants` gives it.
    pub fn new(keys: Arc<dyn Keys>, grants: Arc<dyn Grants>) -> Self {
        Self { keys, grants }
    }
}

/// The protocol tokens `grant` holds a permission for: what a node offers a peer with that grant.
pub fn offered(grant: &Grant) -> Vec<&'static str> {
    PROTOCOLS
        .iter()
        .filter(|(_, needs)| needs.iter().any(|permission| grant.allows(*permission)))
        .map(|(token, _)| *token)
        .collect()
}

impl PeerStore for NodePeers {
    fn get(&self, peer: &str) -> Result<Option<PeerRecord>, Error> {
        let Ok(id) = peer.parse::<PeerId>() else {
            return Ok(None);
        };
        if !self.keys.is_accepted(&id) {
            return Ok(None);
        }
        let grant = self
            .grants
            .grant(&id)
            .map_err(|error| Error::Identity(error.to_string()))?;
        Ok(Some(
            offered(&grant)
                .into_iter()
                .fold(PeerRecord::new(peer), |record, token| {
                    record.granting(token)
                }),
        ))
    }

    fn put(&self, _record: &PeerRecord) -> Result<(), Error> {
        Err(Error::Identity(
            "this node's peers are changed where its owner keeps them".to_owned(),
        ))
    }

    fn remove(&self, _peer: &str) -> Result<(), Error> {
        Err(Error::Identity(
            "this node's peers are changed where its owner keeps them".to_owned(),
        ))
    }
}

/// The admission of `protocol`: the peer's grant, asked of `grants` now, must hold at least one of the
/// permissions [`PROTOCOLS`] names for it and must still cover what the stream's open asked for
/// ([`covers`]). A grant that cannot be read refuses.
pub fn admission(grants: Arc<dyn Grants>, protocol: &'static str) -> Admission {
    let needs = PROTOCOLS
        .iter()
        .find(|(token, _)| *token == protocol)
        .map_or(&[][..], |(_, needs)| *needs);
    Box::new(move |record: &PeerRecord, metadata: &[u8]| {
        let grant = grant_of(grants.as_ref(), &record.peer)
            .map_err(|_| "the host cannot read its peer grants")?;
        if !needs.iter().any(|permission| grant.allows(*permission)) {
            return Err("this peer may not do that");
        }
        if covers(&grant, protocol, metadata) {
            Ok(())
        } else {
            Err("this peer's grant does not cover what the stream asked for")
        }
    })
}

/// Whether `grant` still covers the request a stream of `protocol` opened with (`metadata`): the program
/// of an exec or a shell is one a program list holds, and the file of a transfer lies under a root. A
/// request that cannot be read is covered, so its engine answers it; a control stream decides each call
/// as it comes ([`crate::control::Context::current`]).
pub fn covers(grant: &Grant, protocol: &str, metadata: &[u8]) -> bool {
    match protocol {
        protocol::EXEC => crate::exec::covers(grant, metadata),
        protocol::SHELL => crate::shell::covers(grant, metadata),
        protocol::READ => crate::fs::covers_read(grant, metadata),
        protocol::WRITE => crate::fs::covers_write(grant, metadata),
        protocol::CONNECT => crate::forward::covers(grant, metadata),
        protocol::TCP => crate::forward::covers_tcp(grant, metadata),
        protocol::GUI => crate::gui::covers(grant, metadata),
        protocol::LISTEN => crate::forward::covers_listen(grant, metadata),
        _ => true,
    }
}

/// The grant `grants` gives `peer`, named by its text.
///
/// # Errors
///
/// [`GrantsError`] when `peer` is not an id or `grants` cannot be read.
pub fn grant_of(grants: &dyn Grants, peer: &str) -> Result<Grant, GrantsError> {
    let id = peer
        .parse::<PeerId>()
        .map_err(|_| GrantsError::new("that is not a peer id"))?;
    grants.grant(&id)
}
