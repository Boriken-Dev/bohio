//! The host's keys and peers: its identity, the trust store, `peers.cfg`, and the commands that change them.
//!
//! The data directory holds `identity` (this host's key), `peers/` (the trust store: `accepted/` and
//! `waiting/`) and `peers.cfg` (what each accepted peer may do). A key that proved itself and is not
//! accepted waits in `peers/waiting/` until [`approve`] or [`revoke`] decides it.

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use bohio_client::link::{Settings, socket};
use coqui_link::auth::{Identity, Method};
use coqui_link::pairing;
use coqui_peer::{KeyMaterial, PeerId, TrustStore};
use coqui_transport::Channel;

use crate::link::{Grants, GrantsError, Keys};
use crate::policy::{Grant, Policies, PolicyError};

/// Why a key or peer operation failed. Never holds a key, a PIN or a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PeerError {
    /// The identity could not be read or made.
    #[error("the identity cannot be used: {0}")]
    Identity(String),
    /// The trust store could not be opened or changed.
    #[error("the trust store cannot be used: {0}")]
    Store(String),
    /// `peers.cfg` could not be read, written or understood.
    #[error("{0}")]
    Policy(PolicyError),
    /// `peers.cfg` could not be written.
    #[error("peers.cfg cannot be written: {0}")]
    Write(String),
    /// No such peer is accepted or waiting.
    #[error("no accepted or waiting peer has that id")]
    NoSuchPeer,
    /// Pairing did not finish.
    #[error("pairing failed: {0}")]
    Pairing(String),
}

impl From<PolicyError> for PeerError {
    fn from(error: PolicyError) -> Self {
        Self::Policy(error)
    }
}

/// A host's data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Data {
    dir: PathBuf,
}

impl Data {
    /// The data directory `dir`; nothing is created until it is used.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory itself.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where this host's key is kept.
    pub fn identity_path(&self) -> PathBuf {
        self.dir.join("identity")
    }

    /// Where the per-peer grants are kept.
    pub fn policy_path(&self) -> PathBuf {
        self.dir.join("peers.cfg")
    }

    /// This host's identity, made (a Noise key) when none exists yet.
    ///
    /// # Errors
    ///
    /// [`PeerError::Identity`] when the directory or the file cannot be used.
    pub fn identity(&self) -> Result<Identity, PeerError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| PeerError::Identity(error.to_string()))?;
        Identity::open_or_create(&self.identity_path(), Method::DEFAULT)
            .map_err(|error| PeerError::Identity(error.to_string()))
    }

    /// The trust store, made when none exists yet.
    ///
    /// # Errors
    ///
    /// [`PeerError::Store`] when the directory cannot be used.
    pub fn store(&self) -> Result<TrustStore, PeerError> {
        std::fs::create_dir_all(&self.dir).map_err(|error| PeerError::Store(error.to_string()))?;
        TrustStore::open(&self.dir).map_err(|error| PeerError::Store(error.to_string()))
    }

    /// The grants in `peers.cfg`; none when the file is missing.
    ///
    /// # Errors
    ///
    /// [`PeerError::Policy`] when the file is not valid.
    pub fn policies(&self) -> Result<Policies, PeerError> {
        Ok(Policies::load(&self.policy_path())?)
    }

    fn save(&self, policies: &Policies) -> Result<(), PeerError> {
        coqui_peer::file::replace(
            &self.policy_path(),
            policies.to_toml().as_bytes(),
            coqui_peer::file::Access::OwnerOnly,
        )
        .map_err(|error| PeerError::Write(error.kind().to_string()))
    }
}

/// An accepted peer and what it may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The peer's id.
    pub id: PeerId,
    /// Its grant: nothing when `peers.cfg` has no entry for it.
    pub grant: Grant,
}

/// A proven key that is not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// The key's id.
    pub id: PeerId,
    /// Where it connected from, when known.
    pub source: Option<SocketAddr>,
}

/// Everything `bohio peer list` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The accepted peers.
    pub accepted: Vec<Accepted>,
    /// The proven keys waiting to be approved.
    pub waiting: Vec<Waiting>,
    /// Ids with a grant in `peers.cfg` that the trust store does not accept: they hold nothing.
    pub orphaned: Vec<String>,
}

/// The accepted and waiting peers.
///
/// # Errors
///
/// [`PeerError::Store`] or [`PeerError::Policy`].
pub fn list(data: &Data) -> Result<Listing, PeerError> {
    let store = data.store()?;
    let policies = data.policies()?;
    let accepted: Vec<Accepted> = store
        .accepted()
        .map_err(|error| PeerError::Store(error.to_string()))?
        .into_iter()
        .map(|id| Accepted {
            grant: policies.grant_for(&id.to_string()),
            id,
        })
        .collect();
    let waiting = store
        .waiting()
        .map_err(|error| PeerError::Store(error.to_string()))?
        .into_iter()
        .map(|entry| Waiting {
            id: entry.id,
            source: entry.source,
        })
        .collect();
    let orphaned = policies
        .peers()
        .filter(|name| {
            name.parse::<PeerId>()
                .map_or(true, |id| !accepted.iter().any(|known| known.id == id))
        })
        .map(str::to_owned)
        .collect();
    Ok(Listing {
        accepted,
        waiting,
        orphaned,
    })
}

/// Accepts `id` (a waiting or already accepted peer) and, when `grant` is given, makes it exactly what
/// the peer may do; without one an existing grant is left as it is, and a new peer holds nothing.
///
/// # Errors
///
/// [`PeerError::NoSuchPeer`] for an id neither waiting nor accepted, or a store or file error.
pub fn approve(data: &Data, id: &PeerId, grant: Option<Grant>) -> Result<(), PeerError> {
    let store = data.store()?;
    if !store.is_accepted(id) {
        let waiting = store
            .waiting()
            .map_err(|error| PeerError::Store(error.to_string()))?;
        if !waiting.iter().any(|entry| entry.id == *id) {
            return Err(PeerError::NoSuchPeer);
        }
        store
            .accept(id)
            .map_err(|error| PeerError::Store(error.to_string()))?;
    }
    if let Some(grant) = grant {
        let mut policies = data.policies()?;
        policies.set(&id.to_string(), grant);
        data.save(&policies)?;
    }
    Ok(())
}

/// What [`revoke`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Revoked {
    /// The peer's grant in `peers.cfg`.
    pub grant: bool,
    /// The peer's key in the trust store, accepted or waiting.
    pub key: bool,
}

/// Takes away everything `id` has: its grant first, so a running stream is ended at the next re-check
/// even before the link notices the key is gone, then its key.
///
/// # Errors
///
/// [`PeerError::NoSuchPeer`] when nothing was removed, or a store or file error.
pub fn revoke(data: &Data, id: &PeerId) -> Result<Revoked, PeerError> {
    let mut policies = data.policies()?;
    let grant = policies.remove(&id.to_string());
    if grant {
        data.save(&policies)?;
    }
    let key = data
        .store()?
        .remove(id)
        .map_err(|error| PeerError::Store(error.to_string()))?;
    if grant || key {
        Ok(Revoked { grant, key })
    } else {
        Err(PeerError::NoSuchPeer)
    }
}

/// Shows a PIN on `listener` and pairs the first peer that joins with it: the joiner's key is accepted
/// (it holds nothing until [`approve`] gives it a grant) and the host's key is stored by the joiner.
///
/// `shown` is called once with the PIN, for the person who reads it out; it is never logged. Returns
/// the joiner's id.
///
/// # Errors
///
/// [`PeerError::Pairing`] when the PIN is used up or expires, nobody pairs within `timeout`, or the
/// listener fails.
pub fn pair(
    data: &Data,
    listener: &TcpListener,
    settings: &Settings,
    timeout: Duration,
    shown: impl FnOnce(&str),
) -> Result<PeerId, PeerError> {
    const POLL: Duration = Duration::from_millis(50);
    let identity = data.identity()?;
    let store = data.store()?;
    let desk = pairing::desk().map_err(|error| PeerError::Pairing(error.to_string()))?;
    let pin = desk
        .show_pin()
        .map_err(|error| PeerError::Pairing(error.to_string()))?;
    shown(pin.pin.as_str());
    drop(pin);
    listener
        .set_nonblocking(true)
        .map_err(|error| PeerError::Pairing(error.to_string()))?;
    let until = Instant::now() + timeout;
    let (report, reports) = mpsc::channel();
    thread::scope(|scope| {
        let mut running = 0_usize;
        loop {
            while let Ok(result) = reports.try_recv() {
                running -= 1;
                match result {
                    Ok(id) => return Ok(id),
                    Err(why) => {
                        tracing::info!("a pairing attempt failed: {why}");
                        if desk.shown_pin().is_none() {
                            return Err(PeerError::Pairing(
                                "the PIN is used up or expired: start again".to_owned(),
                            ));
                        }
                    }
                }
            }
            if Instant::now() >= until {
                return Err(PeerError::Pairing("nobody paired in time".to_owned()));
            }
            match listener.accept() {
                Ok((stream, source)) => {
                    if running >= 16 {
                        continue;
                    }
                    running += 1;
                    let (identity, store, desk, report) =
                        (&identity, &store, &desk, report.clone());
                    scope.spawn(move || {
                        let result = stream
                            .set_nonblocking(false)
                            .and_then(|()| socket(stream, settings))
                            .map_err(|error| error.to_string())
                            .and_then(|channel| {
                                let paired = pairing::respond(
                                    &channel,
                                    identity,
                                    store,
                                    desk,
                                    Some(source),
                                    settings.proof_timeout,
                                );
                                channel.close();
                                paired.map_err(|error| error.to_string())
                            })
                            .map(|paired| paired.peer.id());
                        let _ = report.send(result);
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(POLL);
                }
                Err(error) => return Err(PeerError::Pairing(error.to_string())),
            }
        }
    })
}

/// The data directory's trust store and `peers.cfg` as a node's [`Keys`] and [`Grants`], read afresh at
/// every question so an edit applies at the next open or re-check.
#[derive(Debug, Clone)]
pub struct FilePeers {
    store: TrustStore,
    policy: PathBuf,
}

impl FilePeers {
    /// The peers `data` holds.
    ///
    /// # Errors
    ///
    /// [`PeerError::Store`] when the trust store cannot be opened.
    pub fn open(data: &Data) -> Result<Self, PeerError> {
        Ok(Self {
            store: data.store()?,
            policy: data.policy_path(),
        })
    }
}

impl Keys for FilePeers {
    fn is_accepted(&self, peer: &PeerId) -> bool {
        self.store.is_accepted(peer)
    }

    fn stranger(&self, _peer: &PeerId, key: &KeyMaterial, source: Option<SocketAddr>) {
        self.store.record_waiting(key, source);
    }
}

impl Grants for FilePeers {
    fn grant(&self, peer: &PeerId) -> Result<Grant, GrantsError> {
        Policies::load(&self.policy)
            .map(|policies| policies.grant_for(&peer.to_string()))
            .map_err(|error| GrantsError::new(error.to_string()))
    }
}
