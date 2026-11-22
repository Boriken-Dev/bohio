//! Forwards a node keeps up toward a peer: the `[peer."<id>".forward] local` and `remote` lists.
//!
//! A local entry binds its listener once and keeps it while the entry stays in the file: a connection that arrives while the link
//! is down waits up to `[forward] wait_for_link` for it and is closed after. A remote entry asks the peer's host to listen each
//! time a link to it comes up, for the port the host chose the first time, and again after every loss.
//!
//! [`run`] reads the entries again every `recheck` (a [`Declarations`] answers them), so an edit applies as a grant does: an
//! entry added is started, an entry removed is stopped and its listener closed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, ScopedJoinHandle};
use std::time::{Duration, Instant};

use bohio_client::Error;
use bohio_client::forward::{Ended, ForwardSettings, RemoteForward};
use bohio_client::keep::{Backoff, Jitter, Links, Relink};
use bohio_proto::forward::Bind;
use coqui_peer::PeerId;

use super::spec::{LocalSpec, RemoteSpec};
use crate::link::{KeptDirection, KeptState, NodeEvent, NodeEvents};
use crate::policy::Peers;

/// The forwards one peer's table keeps up, parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct Declared {
    /// The peer.
    pub peer: PeerId,
    /// Its `local` entries.
    pub local: Vec<LocalSpec>,
    /// Its `remote` entries.
    pub remote: Vec<RemoteSpec>,
    /// The waits before the next attempt after a failure: the table's `[dial]` retry settings.
    pub backoff: Backoff,
    /// What was not an entry, as text: told once, and the rest of the table is kept.
    pub problems: Vec<String>,
}

impl Declared {
    /// What `peers` keeps up, one per peer with a `local` or `remote` entry.
    #[must_use]
    pub fn from_peers(peers: &Peers) -> Vec<Self> {
        let mut found = Vec::new();
        for (name, table) in peers.declared() {
            let Ok(peer) = name.parse::<PeerId>() else {
                continue;
            };
            let mut declared = Self {
                peer,
                local: Vec::new(),
                remote: Vec::new(),
                backoff: table.dial().backoff(),
                problems: Vec::new(),
            };
            for entry in table.forward_local() {
                match entry.parse::<LocalSpec>() {
                    Ok(spec) => declared.local.push(spec),
                    Err(error) => declared
                        .problems
                        .push(format!("peer.{name}.forward.local `{entry}`: {error}")),
                }
            }
            for entry in table.forward_remote() {
                match entry.parse::<RemoteSpec>() {
                    Ok(spec) => declared.remote.push(spec),
                    Err(error) => declared
                        .problems
                        .push(format!("peer.{name}.forward.remote `{entry}`: {error}")),
                }
            }
            found.push(declared);
        }
        found
    }
}

/// Where a node reads the forwards it keeps, again at every `recheck`.
pub trait Declarations: Send + Sync {
    /// The forwards to keep now, or why they cannot be read (the ones running keep running).
    ///
    /// # Errors
    ///
    /// The reason the entries could not be read.
    fn declared(&self) -> Result<Vec<Declared>, String>;
}

/// What [`run`] needs besides the entries.
pub(crate) struct Keeper<'a> {
    /// The link to a peer, when one is up.
    pub links_for: &'a (dyn Fn(PeerId) -> Arc<dyn Links> + Sync),
    pub settings: ForwardSettings,
    pub recheck: Duration,
    pub poll: Duration,
    pub events: &'a dyn NodeEvents,
}

/// What one task is: the peer and the entry as written.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    peer: PeerId,
    direction: KeptDirection,
    text: String,
}

struct Running<'scope> {
    stop: Arc<AtomicBool>,
    handle: ScopedJoinHandle<'scope, ()>,
}

/// Keeps what `declarations` lists until `stop` is set.
pub(crate) fn run(keeper: &Keeper<'_>, declarations: &dyn Declarations, stop: &AtomicBool) {
    // What each entry last failed with, so a listener that cannot be bound is told once and not at every recheck.
    let failed: Mutex<HashMap<Key, String>> = Mutex::new(HashMap::new());
    thread::scope(|scope| {
        let mut running: HashMap<Key, Running<'_>> = HashMap::new();
        let mut told: Option<String> = None;
        let mut problems: Vec<String> = Vec::new();
        while !stop.load(Ordering::SeqCst) {
            match declarations.declared() {
                Ok(declared) => {
                    told = None;
                    let mut wanted: HashMap<Key, (Backoff, Entry)> = HashMap::new();
                    for peer in declared {
                        for problem in &peer.problems {
                            if !problems.contains(problem) {
                                tracing::warn!("{problem}");
                                problems.push(problem.clone());
                            }
                        }
                        for spec in peer.local {
                            wanted.insert(
                                Key {
                                    peer: peer.peer,
                                    direction: KeptDirection::Local,
                                    text: spec.text().to_owned(),
                                },
                                (peer.backoff, Entry::Local(spec)),
                            );
                        }
                        for spec in peer.remote {
                            wanted.insert(
                                Key {
                                    peer: peer.peer,
                                    direction: KeptDirection::Remote,
                                    text: spec.text().to_owned(),
                                },
                                (peer.backoff, Entry::Remote(spec)),
                            );
                        }
                    }
                    running.retain(|key, task| {
                        // An entry that is gone from the file stops; one whose task ended (a listener that could not be
                        // bound) is started again below.
                        if !wanted.contains_key(key) {
                            task.stop.store(true, Ordering::SeqCst);
                            return false;
                        }
                        !task.handle.is_finished()
                    });
                    for (key, (backoff, entry)) in wanted {
                        if running.contains_key(&key) {
                            continue;
                        }
                        let task_stop = Arc::new(AtomicBool::new(false));
                        let handle = {
                            let (task_stop, key) = (Arc::clone(&task_stop), key.clone());
                            let failed = &failed;
                            scope.spawn(move || {
                                let task = Task {
                                    keeper,
                                    key: &key,
                                    stop: &task_stop,
                                    backoff,
                                    failed,
                                };
                                match entry {
                                    Entry::Local(spec) => task.local(&spec),
                                    Entry::Remote(spec) => task.remote(&spec),
                                }
                            })
                        };
                        running.insert(
                            key,
                            Running {
                                stop: task_stop,
                                handle,
                            },
                        );
                    }
                }
                Err(reason) => {
                    if told.as_deref() != Some(reason.as_str()) {
                        tracing::warn!("the forwards to keep cannot be read: {reason}");
                        told = Some(reason);
                    }
                }
            }
            sleep_until(stop, keeper.recheck, keeper.poll);
        }
        for task in running.values() {
            task.stop.store(true, Ordering::SeqCst);
        }
    });
}

enum Entry {
    Local(LocalSpec),
    Remote(RemoteSpec),
}

/// Sleeps `wait`, or until `stop`.
fn sleep_until(stop: &AtomicBool, wait: Duration, poll: Duration) {
    let until = Instant::now() + wait;
    while Instant::now() < until && !stop.load(Ordering::SeqCst) {
        thread::sleep(poll.min(until.saturating_duration_since(Instant::now())));
    }
}

struct Task<'a> {
    keeper: &'a Keeper<'a>,
    key: &'a Key,
    stop: &'a AtomicBool,
    backoff: Backoff,
    failed: &'a Mutex<HashMap<Key, String>>,
}

impl Task<'_> {
    fn tell(&self, state: KeptState) {
        self.keeper.events.event(&NodeEvent::Kept {
            peer: self.key.peer,
            direction: self.key.direction,
            spec: self.key.text.clone(),
            state,
        });
    }

    /// Binds the entry's listener and serves it until the entry is removed: the listener outlives every link.
    fn local(&self, spec: &LocalSpec) {
        let settings = &self.keeper.settings;
        let (bound, shown) = match spec.bind(settings) {
            Ok(bound) => bound,
            Err(reason) => {
                let new = self
                    .failed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(self.key.clone(), reason.clone())
                    .as_deref()
                    != Some(reason.as_str());
                if new {
                    self.tell(KeptState::Failed {
                        reason,
                        retry_in: self.keeper.recheck,
                    });
                }
                return;
            }
        };
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(self.key);
        if let Some(address) = spec.non_default() {
            tracing::warn!(
                peer = %self.key.peer,
                "non-default: bind {address}: whoever can reach it has what this peer is granted"
            );
        }
        self.tell(KeptState::Up { address: shown });
        let relink = Relink::new(
            (self.keeper.links_for)(self.key.peer),
            settings.wait_for_link,
            settings.options.listen_poll,
        );
        let report = |error: &Error| {
            tracing::info!(
                peer = %self.key.peer,
                entry = %self.key.text,
                "a forwarded connection failed: {error}"
            );
        };
        let ended = bound.serve(&relink, spec.target(), settings, self.stop, &report);
        match ended {
            Ok(_) => self.tell(KeptState::Down),
            Err(error) => self.tell(KeptState::Failed {
                reason: format!("the listener failed: {error}"),
                retry_in: self.keeper.recheck,
            }),
        }
    }

    /// Asks the peer's host to listen whenever a link is up, for the same port each time.
    fn remote(&self, spec: &RemoteSpec) {
        let links = (self.keeper.links_for)(self.key.peer);
        let report: Arc<dyn Fn(&Error) + Send + Sync> = {
            let (peer, entry) = (self.key.peer, self.key.text.clone());
            Arc::new(move |error: &Error| {
                tracing::info!(%peer, %entry, "a forwarded connection failed: {error}");
            })
        };
        keep_remote(
            &RemoteKeep {
                links: &*links,
                settings: &self.keeper.settings,
                backoff: &self.backoff,
                poll: self.keeper.poll,
                stop: self.stop,
                retry_closed: true,
                tell: &|state| self.tell(state),
                report,
            },
            spec,
            None,
        );
    }
}

/// What [`keep_remote`] works with.
pub struct RemoteKeep<'a> {
    /// The link to the host, when one is up.
    pub links: &'a dyn Links,
    /// The forward's bounds.
    pub settings: &'a ForwardSettings,
    /// How long to wait after a failure.
    pub backoff: &'a Backoff,
    /// How soon it looks for a link that came back.
    pub poll: Duration,
    /// Ends it.
    pub stop: &'a AtomicBool,
    /// Whether a listener the host ended while the link stayed up (its grant no longer covers the bind, or it failed) is asked
    /// for again, as a lost link is. A node keeping a declared entry asks again; a command that was told `no` ends.
    pub retry_closed: bool,
    /// Hears each change of state.
    pub tell: &'a (dyn Fn(KeptState) + Sync),
    /// Hears a connection that could not be bridged.
    pub report: Arc<dyn Fn(&Error) + Send + Sync>,
}

/// Has the peer's host listen for `spec` on every link that comes up until `stop`: the first request asks for what the entry
/// says (`first`, when the caller already made it), each later one for the port the host chose the first time. A request the
/// host refuses is made again after the backoff; a listener that was up and ended (the link was lost, or the host ended it) is
/// asked for again as soon as a link is up. Returns `Some(Ended::Closed)` when the host ended the listener with the link up and
/// `retry_closed` is not set, and `None` when `stop` ended it.
pub fn keep_remote(
    keep: &RemoteKeep<'_>,
    spec: &RemoteSpec,
    mut first: Option<RemoteForward>,
) -> Option<Ended> {
    let mut bind = spec.bind().clone();
    let mut jitter = Jitter::new();
    let mut failures = 0_u32;
    let mut told: Option<String> = None;
    while !keep.stop.load(Ordering::SeqCst) {
        let began = Instant::now();
        let opened = match first.take() {
            Some(forward) => Ok(forward),
            None => {
                let Some(link) = keep.links.current() else {
                    sleep_until(keep.stop, keep.poll, keep.poll);
                    continue;
                };
                RemoteForward::open(
                    link,
                    &bind,
                    spec.local(),
                    keep.settings,
                    Arc::clone(&keep.report),
                )
            }
        };
        match opened {
            Ok(forward) => {
                bind = same_port(&bind, forward.address());
                told = None;
                (keep.tell)(KeptState::Up {
                    address: forward.address().to_owned(),
                });
                while !keep.stop.load(Ordering::SeqCst) && forward.ended().is_none() {
                    thread::sleep(keep.settings.options.listen_poll);
                }
                let ended = forward.ended();
                forward.close();
                (keep.tell)(KeptState::Down);
                if ended == Some(Ended::Closed) && !keep.retry_closed {
                    return ended;
                }
                // A listener that lasted at least `retry_min` starts the waits again, and the next request is made as
                // soon as a link is up; one the host ended at once is a failure that waits.
                if began.elapsed() >= keep.backoff.min {
                    failures = 0;
                } else {
                    failures = failures.saturating_add(1);
                    sleep_until(
                        keep.stop,
                        keep.backoff.wait(failures, jitter.sample()),
                        keep.poll,
                    );
                }
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                let wait = keep.backoff.wait(failures, jitter.sample());
                let reason = error.to_string();
                if told.as_deref() != Some(reason.as_str()) {
                    (keep.tell)(KeptState::Failed {
                        reason: reason.clone(),
                        retry_in: wait,
                    });
                    told = Some(reason);
                }
                sleep_until(keep.stop, wait, keep.poll);
            }
        }
    }
    None
}

/// `bind` with the port the host chose (`address`, as `HOST:PORT`): the next request asks for that one.
pub(crate) fn same_port(bind: &Bind, address: &str) -> Bind {
    match bind {
        Bind::Tcp { host, .. } => match address
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
        {
            Some(port) => Bind::Tcp {
                host: host.clone(),
                port,
            },
            None => bind.clone(),
        },
        other => other.clone(),
    }
}
