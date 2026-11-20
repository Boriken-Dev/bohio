//! The host's keys and peers: its identity, the waiting list, the peers of `bohio.cfg` and the commands
//! that change them.
//!
//! A peer is a `[peer.<id>]` table in `bohio.cfg`, which is what accepts its key; `allow` says what it may
//! do. The data directory holds what is state, not configuration: `identity` (this host's key) and
//! `peers/waiting/`, the proven keys that were not accepted, until [`approve`] or [`revoke`] decides them.

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use bohio_client::link::{Settings, socket};
use coqui_link::auth::{Identity, Method};
use coqui_link::pairing;
use coqui_peer::{KeyMaterial, PeerId, TrustStore};
use coqui_transport::Channel;
use toml_edit::{DocumentMut, Item, Table};

use crate::config::{ConfigError, Layers};
use crate::link::{Grants, GrantsError, Keys};
use crate::policy::{Grant, Peers};

/// Why a key or peer operation failed. Never holds a key, a PIN or a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PeerError {
    /// The identity could not be read or made.
    #[error("the identity cannot be used: {0}")]
    Identity(String),
    /// The waiting list could not be opened or changed.
    #[error("the trust store cannot be used: {0}")]
    Store(String),
    /// A configuration layer could not be read or understood.
    #[error("{0}")]
    Config(ConfigError),
    /// A configuration file could not be written.
    #[error("{0}")]
    Write(String),
    /// No configuration file can be written, and none was named to make.
    #[error("no configuration file can be written: name one with --config")]
    NoLayer,
    /// No accepted or waiting peer has that id.
    #[error("no accepted or waiting peer has that id")]
    NoSuchPeer,
    /// Pairing did not finish.
    #[error("pairing failed: {0}")]
    Pairing(String),
}

impl From<ConfigError> for PeerError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
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

    /// The waiting list (the trust store; its `accepted/` is not read), made when none exists yet.
    ///
    /// # Errors
    ///
    /// [`PeerError::Store`] when the directory cannot be used.
    pub fn store(&self) -> Result<TrustStore, PeerError> {
        std::fs::create_dir_all(&self.dir).map_err(|error| PeerError::Store(error.to_string()))?;
        TrustStore::open(&self.dir).map_err(|error| PeerError::Store(error.to_string()))
    }
}

/// An accepted peer and what it may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The peer's id.
    pub id: PeerId,
    /// Its grant: nothing when its table has no `allow`.
    pub grant: Grant,
    /// Where this node dials the peer, when its table has an `endpoint`.
    pub endpoint: Option<String>,
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
}

/// The accepted peers of the layers `files` and the keys waiting in `data`.
///
/// # Errors
///
/// [`PeerError::Config`] for a layer that cannot be read, [`PeerError::Store`] for the waiting list.
pub fn list(files: &[PathBuf], data: &Data) -> Result<Listing, PeerError> {
    let peers = Layers::peers(files)?;
    let accepted = peers
        .ids()
        .filter_map(|name| {
            name.parse::<PeerId>().ok().map(|id| Accepted {
                grant: peers.grant_for(name),
                endpoint: peers
                    .table(name)
                    .and_then(|table| table.endpoint().map(str::to_owned)),
                id,
            })
        })
        .collect();
    let waiting = data
        .store()?
        .waiting()
        .map_err(|error| PeerError::Store(error.to_string()))?
        .into_iter()
        .map(|entry| Waiting {
            id: entry.id,
            source: entry.source,
        })
        .collect();
    Ok(Listing { accepted, waiting })
}

/// The peers of the layers `files`, read again at every question, as a node's [`Keys`] and [`Grants`]: a
/// hand edit of any layer applies at the next open or re-check. A layer that cannot be read or parsed
/// refuses everything, and says so once for each distinct error. A proven stranger is recorded as waiting
/// in the data directory's trust store.
#[derive(Debug)]
pub struct LayeredPeers {
    files: Vec<PathBuf>,
    store: TrustStore,
    reported: Mutex<Option<String>>,
}

impl LayeredPeers {
    /// The peers of `files` (lowest layer first; one that does not exist is skipped, so a layer made
    /// later applies), strangers recorded in `data`.
    ///
    /// # Errors
    ///
    /// [`PeerError::Store`] when the waiting list cannot be opened.
    pub fn new(files: Vec<PathBuf>, data: &Data) -> Result<Self, PeerError> {
        Ok(Self {
            files,
            store: data.store()?,
            reported: Mutex::new(None),
        })
    }

    fn current(&self) -> Result<Peers, GrantsError> {
        match Layers::peers(&self.files) {
            Ok(peers) => {
                *self.reported.lock().unwrap_or_else(PoisonError::into_inner) = None;
                Ok(peers)
            }
            Err(error) => {
                let text = error.to_string();
                let mut last = self.reported.lock().unwrap_or_else(PoisonError::into_inner);
                if last.as_deref() != Some(text.as_str()) {
                    tracing::warn!("every peer is refused until this is fixed: {text}");
                    *last = Some(text.clone());
                }
                Err(GrantsError::new(text))
            }
        }
    }
}

impl Keys for LayeredPeers {
    fn is_accepted(&self, peer: &PeerId) -> bool {
        self.current()
            .is_ok_and(|peers| peers.has(&peer.to_string()))
    }

    fn stranger(&self, _peer: &PeerId, key: &KeyMaterial, source: Option<SocketAddr>) {
        self.store.record_waiting(key, source);
    }
}

impl Grants for LayeredPeers {
    fn grant(&self, peer: &PeerId) -> Result<Grant, GrantsError> {
        Ok(self.current()?.grant_for(&peer.to_string()))
    }
}

/// Where [`approve`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approved {
    /// The layer file that holds the peer's table now.
    pub file: PathBuf,
    /// Whether the file was made by this call.
    pub made: bool,
}

/// Accepts `id`: its `[peer.<id>]` table is written to a layer, and its waiting entry (and any stored key)
/// removed. With `grant` the table holds exactly that grant (and keeps its `endpoint` and `[dial]`); without one an existing table stays as it is
/// and a new peer holds nothing. `endpoint`, when given, is written as the table's `endpoint`: this node then dials the
/// peer. Every other byte of the file, its comments and layout, stays.
///
/// The layer is the one that already holds the table; else the last of `files` this account can write; else
/// `create` (parents made, readable by the owner alone).
///
/// # Errors
///
/// [`PeerError::NoLayer`] when no file can take the table, [`PeerError::Write`] for a file that cannot be
/// written, [`PeerError::Config`] for a layer that is not valid TOML, or a store error.
pub fn approve(
    files: &[PathBuf],
    create: Option<&Path>,
    data: &Data,
    id: &PeerId,
    grant: Option<&Grant>,
    endpoint: Option<&str>,
) -> Result<Approved, PeerError> {
    let name = id.to_string();
    let (file, made) = target(files, create, &name)?;
    let mut document = if made {
        DocumentMut::new()
    } else {
        read_document(&file)?
    };
    let before = document.to_string();
    set_peer(&mut document, &name, grant, endpoint)?;
    let after = document.to_string();
    if made || after != before {
        write_document(&file, &after, made)?;
    }
    data.store()?
        .remove(id)
        .map_err(|error| PeerError::Store(error.to_string()))?;
    Ok(Approved { file, made })
}

/// What [`revoke`] removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revoked {
    /// The layers whose `[peer.<id>]` table was removed.
    pub files: Vec<PathBuf>,
    /// Layers that hold the table and cannot be written: the key stays accepted there.
    pub unwritable: Vec<PathBuf>,
    /// The key waiting or stored in the data directory.
    pub key: bool,
}

/// Takes away everything `id` has: its table from every layer that holds one, then its waiting entry. A
/// running stream ends at the next re-check, even before the link notices the key is gone.
///
/// # Errors
///
/// [`PeerError::NoSuchPeer`] when nothing was removed and no layer holds it, [`PeerError::Write`] for a file
/// that cannot be replaced, or a layer or store error.
pub fn revoke(files: &[PathBuf], data: &Data, id: &PeerId) -> Result<Revoked, PeerError> {
    let name = id.to_string();
    let (mut removed, mut unwritable) = (Vec::new(), Vec::new());
    for file in files.iter().rev() {
        if !file.is_file() {
            continue;
        }
        let mut document = read_document(file)?;
        if !remove_peer(&mut document, &name) {
            continue;
        }
        if writable(file) {
            write_document(file, &document.to_string(), false)?;
            removed.push(file.clone());
        } else {
            unwritable.push(file.clone());
        }
    }
    let key = data
        .store()?
        .remove(id)
        .map_err(|error| PeerError::Store(error.to_string()))?;
    if removed.is_empty() && unwritable.is_empty() && !key {
        return Err(PeerError::NoSuchPeer);
    }
    Ok(Revoked {
        files: removed,
        unwritable,
        key,
    })
}

/// The file `approve` writes, and whether it is to be made.
fn target(
    files: &[PathBuf],
    create: Option<&Path>,
    name: &str,
) -> Result<(PathBuf, bool), PeerError> {
    for file in files.iter().rev() {
        if file.is_file() && holds(file, name)? {
            return if writable(file) {
                Ok((file.clone(), false))
            } else {
                Err(PeerError::Write(format!(
                    "{} holds this peer and cannot be written",
                    file.display()
                )))
            };
        }
    }
    if let Some(file) = files
        .iter()
        .rev()
        .find(|file| file.is_file() && writable(file))
    {
        return Ok((file.clone(), false));
    }
    match create {
        Some(path) if path.is_file() => {
            if writable(path) {
                Ok((path.to_owned(), false))
            } else {
                Err(PeerError::Write(format!(
                    "{} cannot be written",
                    path.display()
                )))
            }
        }
        Some(path) => Ok((path.to_owned(), true)),
        None => Err(PeerError::NoLayer),
    }
}

/// Whether this account can change `path` (opened for appending, so nothing is truncated).
fn writable(path: &Path) -> bool {
    std::fs::OpenOptions::new().append(true).open(path).is_ok()
}

fn read_document(path: &Path) -> Result<DocumentMut, PeerError> {
    let label = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|error| {
        PeerError::Config(ConfigError::Read {
            path: label.clone(),
            reason: error.kind().to_string(),
        })
    })?;
    text.parse::<DocumentMut>().map_err(|error| {
        let (line, column) = error
            .span()
            .map_or((1, 1), |span| crate::config::position(&text, span.start));
        PeerError::Config(ConfigError::Parse {
            path: label,
            line,
            column,
            key: None,
            message: error.message().to_owned(),
        })
    })
}

fn holds(path: &Path, name: &str) -> Result<bool, PeerError> {
    let document = read_document(path)?;
    Ok(document
        .get("peer")
        .and_then(Item::as_table_like)
        .is_some_and(|peers| peers.contains_key(name)))
}

/// Sets `[peer.<name>]` to `grant`, or makes it empty when it is absent and `grant` is `None`; `endpoint`, when
/// given, is written as the table's `endpoint`. A table that is replaced keeps its `endpoint` and `[dial]`.
fn set_peer(
    document: &mut DocumentMut,
    name: &str,
    grant: Option<&Grant>,
    endpoint: Option<&str>,
) -> Result<(), PeerError> {
    let peers = document
        .as_table_mut()
        .entry("peer")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .ok_or_else(|| PeerError::Write("`peer` in that file is not a table".to_owned()))?;
    match grant {
        Some(grant) => {
            let text = toml::to_string(grant)
                .map_err(|_| PeerError::Write("the grant cannot be written".to_owned()))?;
            let mut table = text
                .parse::<DocumentMut>()
                .map_err(|_| PeerError::Write("the grant cannot be written".to_owned()))?
                .into_table();
            if let Some(old) = peers.get(name).and_then(Item::as_table) {
                // The old table's own comments and place in the file stay, and what says where it is dialed.
                *table.decor_mut() = old.decor().clone();
                table.set_position(old.position());
                for key in ["endpoint", "dial"] {
                    if let Some(kept) = old.get(key) {
                        table.insert(key, kept.clone());
                    }
                }
            }
            peers.insert(name, Item::Table(table));
        }
        None => {
            if !peers.contains_key(name) {
                peers.insert(name, Item::Table(Table::new()));
            }
        }
    }
    if let Some(endpoint) = endpoint {
        peers
            .get_mut(name)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| PeerError::Write(format!("`peer.{name}` is not a table")))?
            .insert("endpoint", toml_edit::value(endpoint));
    }
    Ok(())
}

/// Removes `[peer.<name>]` (and an emptied `[peer]`); `true` when it was there.
fn remove_peer(document: &mut DocumentMut, name: &str) -> bool {
    let Some(peers) = document.get_mut("peer").and_then(Item::as_table_mut) else {
        return false;
    };
    let removed = peers.remove(name).is_some();
    if removed && peers.is_empty() {
        document.as_table_mut().remove("peer");
    }
    removed
}

/// Replaces `path` (or makes it) with `text`, atomically through a temporary sibling. A new file is readable
/// by its owner alone; a replaced one keeps its mode on Unix and takes its directory's inherited access list
/// on Windows.
fn write_document(path: &Path, text: &str, made: bool) -> Result<(), PeerError> {
    #[cfg(unix)]
    let mode = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    coqui_peer::file::replace(path, text.as_bytes(), coqui_peer::file::Access::OwnerOnly).map_err(
        |error| {
            PeerError::Write(format!(
                "{} cannot be written: {}",
                path.display(),
                error.kind()
            ))
        },
    )?;
    #[cfg(unix)]
    if !made && let Some(mode) = mode {
        let _ = std::fs::set_permissions(path, mode);
    }
    #[cfg(not(unix))]
    let _ = made;
    Ok(())
}

/// Shows a PIN on `listener` and pairs the first peer that joins with it: the joiner's key is stored
/// (the command then writes its `[peer.<id>]` table with [`approve`], which removes it from the store) and
/// the host's key is stored by the joiner.
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
