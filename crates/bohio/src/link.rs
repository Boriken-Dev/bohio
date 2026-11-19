//! Who may open which protocol: the trust store says who is proven, `peers.cfg` says what each may do.
//!
//! [`TrustPeers`] answers a link's question "who is this?" from the trust store: an accepted key is a
//! peer, and a removed or retired one is not. [`admission`] answers "may it do this?" from `peers.cfg`,
//! read afresh at every open and at every re-check of a running stream, so a revoke ends what it
//! covers within `[link] recheck`.

use std::path::{Path, PathBuf};

use coqui_link::Error;
use coqui_link::link::Admission;
use coqui_link::peers::{Event, Events, PeerRecord, PeerStore};
use coqui_peer::{PeerId, TrustStore};

use bohio_proto::protocol;

use crate::policy::{Permission, Policies};

/// The protocols a host serves, each with the permissions of which a peer needs at least one to open it.
pub const PROTOCOLS: [(&str, &[Permission]); 5] = [
    (
        protocol::CONTROL,
        &[Permission::Info, Permission::FsRead, Permission::FsWrite],
    ),
    (protocol::EXEC, &[Permission::Exec]),
    (protocol::SHELL, &[Permission::Shell]),
    (protocol::READ, &[Permission::FsRead]),
    (protocol::WRITE, &[Permission::FsWrite]),
];

/// The trust store as a link's peer store: an accepted key is a peer that may open every protocol the
/// host serves, and [`admission`] narrows that to what `peers.cfg` grants.
#[derive(Debug, Clone)]
pub struct TrustPeers {
    store: TrustStore,
}

impl TrustPeers {
    /// The peers `store` accepts.
    pub const fn new(store: TrustStore) -> Self {
        Self { store }
    }
}

impl PeerStore for TrustPeers {
    fn get(&self, peer: &str) -> Result<Option<PeerRecord>, Error> {
        let Ok(id) = peer.parse::<PeerId>() else {
            return Ok(None);
        };
        if !self.store.is_accepted(&id) {
            return Ok(None);
        }
        Ok(Some(
            PROTOCOLS
                .iter()
                .fold(PeerRecord::new(peer), |record, (token, _)| {
                    record.granting(token)
                }),
        ))
    }

    fn put(&self, _record: &PeerRecord) -> Result<(), Error> {
        Err(Error::Identity(
            "this host's peers are changed with `bohio peer`".to_owned(),
        ))
    }

    fn remove(&self, _peer: &str) -> Result<(), Error> {
        Err(Error::Identity(
            "this host's peers are changed with `bohio peer`".to_owned(),
        ))
    }
}

/// The admission of one protocol: the peer's grant in the `peers.cfg` at `policy`, read now, must hold
/// at least one of `needs`. A file that cannot be read refuses.
pub fn admission(policy: PathBuf, needs: &'static [Permission]) -> Admission {
    Box::new(move |record: &PeerRecord, _metadata: &[u8]| {
        let policies =
            Policies::load(&policy).map_err(|_| "the host cannot read its peer grants")?;
        let grant = policies.grant_for(&record.peer);
        if needs.iter().any(|permission| grant.allows(*permission)) {
            Ok(())
        } else {
            Err("this peer may not do that")
        }
    })
}

/// The grant of `peer` in the `peers.cfg` at `policy`.
///
/// # Errors
///
/// The text of the error when the file cannot be read or is not valid.
pub fn grant_of(policy: &Path, peer: &str) -> Result<crate::policy::Grant, String> {
    Policies::load(policy)
        .map(|policies| policies.grant_for(peer))
        .map_err(|error| error.to_string())
}

/// Link events as log lines: a peer's id and a protocol name, never a key, a PIN or a request.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogEvents;

impl Events for LogEvents {
    fn event(&self, event: &Event) {
        match event {
            Event::LinkUp { peer } => tracing::info!(%peer, "link up"),
            Event::LinkDown { peer } => tracing::info!(%peer, "link down"),
            Event::Opened { peer, protocol } => tracing::debug!(%peer, %protocol, "stream opened"),
            Event::Refused { peer, protocol } => {
                tracing::info!(%peer, %protocol, "stream refused");
            }
            Event::Revoked { peer, protocol } => {
                tracing::info!(%peer, %protocol, "stream ended: the peer lost its grant");
            }
            _ => {}
        }
    }
}
