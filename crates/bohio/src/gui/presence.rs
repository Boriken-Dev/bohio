//! Presence: telling the person at the screen that a peer views or controls it, and asking them first when the host is
//! configured to.
//!
//! An [`Engagement`] is one peer on one session, from its first GUI stream until `indicator_linger` after its last one
//! closes, so an agent that opens many short streams makes one notice. A node without an application's [`Presence`] uses
//! [`LocalPresence`]: a [`Notifier`] shows the notice, and an approval waits in [`Approvals`] until the node's local
//! endpoint (`bohio gui allow`) or the notice's action decides it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bohio_proto::gui::{PendingRequest, Want};
use coqui_peer::PeerId;

/// One peer's presence on one desktop session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engagement {
    /// The peer.
    pub peer: PeerId,
    /// The desktop session.
    pub session: String,
    /// Whether the peer drives the screen (it holds an input stream) and not only looks.
    pub controlling: bool,
}

/// A stream waiting to be allowed.
#[derive(Debug, Clone, Copy)]
pub struct ApprovalRequest<'a> {
    /// The peer.
    pub peer: PeerId,
    /// The desktop session.
    pub session: &'a str,
    /// What the peer asked for.
    pub want: &'a [Want],
    /// When the request began to wait.
    pub since: SystemTime,
    /// How long it may wait: past it the answer is [`Approval::Deny`].
    pub timeout: Duration,
    /// How often the wait looks at `cancelled` (`[gui] poll_interval`): a peer that gave up is noticed within this.
    pub poll: Duration,
    /// Set when the peer gave up (its stream closed): [`Presence::approve`] returns [`Approval::Deny`] soon after.
    pub cancelled: &'a AtomicBool,
}

/// The person's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Let the stream open.
    Allow,
    /// Refuse it.
    Deny,
}

/// What an application does about its owner's desk: tell them, and ask them.
///
/// Every method runs on the thread of the stream that caused it and may take its time (`approve` waits for a person);
/// none may panic. A [`Presence`] an application gives with `Node::with_presence` replaces [`LocalPresence`], so the node's
/// local endpoint then has no approvals to list.
pub trait Presence: Send + Sync {
    /// A peer's engagement began, or the peer began to control the screen it was only viewing.
    fn engaged(&self, engagement: &Engagement);

    /// The engagement ended, `indicator_linger` after its last stream closed.
    fn released(&self, engagement: &Engagement);

    /// Whether the peer may have the stream it asked for. Called only with `[gui] approval = "local"`, and not again for
    /// the same peer and session within `approval_ttl` of an [`Approval::Allow`].
    fn approve(&self, request: &ApprovalRequest<'_>) -> Approval;
}

/// A notice shown to the person at the screen. Its words come from the host and the peer's id, never from the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The title: `Bohío`.
    pub title: String,
    /// The line: who does what to the screen.
    pub body: String,
}

/// What a notice's Allow and Deny actions call once with the choice.
pub type Decide = Box<dyn FnOnce(bool) + Send>;

/// Where a notice is shown.
pub trait Notifier: Send + Sync {
    /// Shows `notice`. With `decide`, a notifier that has actions offers Allow and Deny and calls it once with the choice; one
    /// that has none drops it (the decision is then made on the node's local endpoint). A notification service that is not
    /// there is the notifier's to log once, never a failure: the indicator informs, it does not gate.
    fn notify(&self, notice: &Notice, decide: Option<Decide>);
}

/// A notifier that writes the notice to the log: what a node uses until it is given a desktop notifier.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogNotifier;

impl Notifier for LogNotifier {
    fn notify(&self, notice: &Notice, _decide: Option<Decide>) {
        tracing::info!(title = %notice.title, body = %notice.body, "notice");
    }
}

/// A peer's id as a notice writes it: the scheme and the first twelve digits.
pub fn short_id(peer: &PeerId) -> String {
    let text = peer.to_string();
    match text.split_once(':') {
        Some((scheme, digits)) => {
            format!("{scheme}:{}", digits.chars().take(12).collect::<String>())
        }
        None => text.chars().take(12).collect(),
    }
}

fn verb(controlling: bool) -> &'static str {
    if controlling {
        "controlling"
    } else {
        "viewing"
    }
}

/// The notice of an engagement that began.
pub fn engaged_notice(engagement: &Engagement) -> Notice {
    Notice {
        title: "Bohío".to_owned(),
        body: format!(
            "peer {} is {} this screen",
            short_id(&engagement.peer),
            verb(engagement.controlling)
        ),
    }
}

/// The notice of an engagement that ended.
pub fn released_notice(engagement: &Engagement) -> Notice {
    Notice {
        title: "Bohío".to_owned(),
        body: format!(
            "peer {} has stopped {} this screen",
            short_id(&engagement.peer),
            verb(engagement.controlling)
        ),
    }
}

/// The notice of a request that waits for the person.
pub fn asking_notice(peer: &PeerId, want: &[Want]) -> Notice {
    let controlling = want.contains(&Want::Input);
    Notice {
        title: "Bohío".to_owned(),
        body: format!(
            "peer {} asks to {} this screen",
            short_id(peer),
            if controlling { "control" } else { "view" }
        ),
    }
}

#[derive(Debug)]
struct Entry {
    id: u64,
    peer: PeerId,
    session: String,
    want: Vec<Want>,
    since: SystemTime,
    decision: Option<bool>,
}

#[derive(Debug, Default)]
struct Waiting {
    next: u64,
    version: u64,
    entries: Vec<Entry>,
}

/// The approvals a node waits for. [`LocalPresence`] fills it; the node's local endpoint lists and decides.
#[derive(Debug, Default)]
pub struct Approvals {
    waiting: Mutex<Waiting>,
    changed: Condvar,
}

impl Approvals {
    fn lock(&self) -> std::sync::MutexGuard<'_, Waiting> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The requests waiting for a decision, oldest first.
    pub fn list(&self) -> Vec<PendingRequest> {
        self.lock()
            .entries
            .iter()
            .filter(|entry| entry.decision.is_none())
            .map(|entry| PendingRequest {
                id: entry.id,
                peer: entry.peer.to_string(),
                session: entry.session.clone(),
                want: entry.want.clone(),
                since: entry
                    .since
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs()),
            })
            .collect()
    }

    /// A number that changes whenever the list does.
    pub fn version(&self) -> u64 {
        self.lock().version
    }

    /// Waits up to `wait` for the list to change from `version`; returns the version then.
    pub fn changed_since(&self, version: u64, wait: Duration) -> u64 {
        let guard = self.lock();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, wait, |waiting| waiting.version == version)
            .unwrap_or_else(PoisonError::into_inner);
        guard.version
    }

    /// Allows or denies request `id`; `false` when it is not waiting (it ended, or was decided already).
    pub fn decide(&self, id: u64, allow: bool) -> bool {
        let mut waiting = self.lock();
        let Some(entry) = waiting
            .entries
            .iter_mut()
            .find(|entry| entry.id == id && entry.decision.is_none())
        else {
            return false;
        };
        entry.decision = Some(allow);
        waiting.version += 1;
        drop(waiting);
        self.changed.notify_all();
        true
    }

    fn submit(&self, request: &ApprovalRequest<'_>) -> u64 {
        let mut waiting = self.lock();
        waiting.next += 1;
        let id = waiting.next;
        waiting.entries.push(Entry {
            id,
            peer: request.peer,
            session: request.session.to_owned(),
            want: request.want.to_vec(),
            since: request.since,
            decision: None,
        });
        waiting.version += 1;
        drop(waiting);
        self.changed.notify_all();
        id
    }

    /// Waits for request `id` to be decided, for `request.timeout` or until `request.cancelled`; the request leaves the
    /// list either way.
    fn wait(&self, id: u64, request: &ApprovalRequest<'_>) -> Approval {
        let deadline = Instant::now() + request.timeout;
        let mut waiting = self.lock();
        let decision = loop {
            let decided = waiting
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .and_then(|entry| entry.decision);
            if let Some(allow) = decided {
                break allow;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || request.cancelled.load(Ordering::SeqCst) {
                break false;
            }
            waiting = self
                .changed
                .wait_timeout(waiting, left.min(request.poll))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        waiting.entries.retain(|entry| entry.id != id);
        waiting.version += 1;
        drop(waiting);
        self.changed.notify_all();
        if decision {
            Approval::Allow
        } else {
            Approval::Deny
        }
    }
}

/// Where a node's notices go, changeable until it serves.
#[derive(Default)]
pub(crate) struct NotifierCell(RwLock<Option<Arc<dyn Notifier>>>);

impl NotifierCell {
    pub(crate) fn set(&self, notifier: Arc<dyn Notifier>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(notifier);
    }

    fn notify(&self, notice: &Notice, decide: Option<Decide>) {
        let notifier = self
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match notifier {
            Some(notifier) => notifier.notify(notice, decide),
            None => LogNotifier.notify(notice, decide),
        }
    }
}

/// What a node does without an application's [`Presence`]: notices through its [`Notifier`], approvals in its
/// [`Approvals`].
pub struct LocalPresence {
    notifier: Arc<NotifierCell>,
    approvals: Arc<Approvals>,
}

impl LocalPresence {
    pub(crate) fn new(notifier: Arc<NotifierCell>, approvals: Arc<Approvals>) -> Self {
        Self {
            notifier,
            approvals,
        }
    }
}

impl core::fmt::Debug for LocalPresence {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalPresence").finish_non_exhaustive()
    }
}

impl Presence for LocalPresence {
    fn engaged(&self, engagement: &Engagement) {
        self.notifier.notify(&engaged_notice(engagement), None);
    }

    fn released(&self, engagement: &Engagement) {
        self.notifier.notify(&released_notice(engagement), None);
    }

    fn approve(&self, request: &ApprovalRequest<'_>) -> Approval {
        let id = self.approvals.submit(request);
        let approvals = Arc::clone(&self.approvals);
        self.notifier.notify(
            &asking_notice(&request.peer, request.want),
            Some(Box::new(move |allow| {
                approvals.decide(id, allow);
            })),
        );
        self.approvals.wait(id, request)
    }
}

type Key = (PeerId, String);

struct Live {
    streams: usize,
    controlling: bool,
    epoch: u64,
}

/// The engagements of a node: who is on which session now, and when a notice is due.
pub(crate) struct Engagements {
    presence: RwLock<Arc<dyn Presence>>,
    linger: Duration,
    enabled: bool,
    live: Mutex<HashMap<Key, Live>>,
}

/// One stream's share of an engagement; dropping it ends the share.
pub(crate) struct Hold {
    engagements: Option<Arc<Engagements>>,
    key: Key,
}

impl Engagements {
    pub(crate) fn new(presence: Arc<dyn Presence>, linger: Duration, enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            presence: RwLock::new(presence),
            linger,
            enabled,
            live: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn set_presence(&self, presence: Arc<dyn Presence>) {
        *self
            .presence
            .write()
            .unwrap_or_else(PoisonError::into_inner) = presence;
    }

    pub(crate) fn presence(&self) -> Arc<dyn Presence> {
        Arc::clone(&self.presence.read().unwrap_or_else(PoisonError::into_inner))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Live>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A stream of `peer` on `session` is open. The presence hears it when the engagement is new, or when it just began to
    /// control.
    pub(crate) fn enter(self: &Arc<Self>, peer: PeerId, session: &str, controlling: bool) -> Hold {
        if !self.enabled {
            return Hold {
                engagements: None,
                key: (peer, session.to_owned()),
            };
        }
        let key = (peer, session.to_owned());
        let announce = {
            let mut live = self.lock();
            let entry = live.entry(key.clone()).or_insert(Live {
                streams: 0,
                controlling: false,
                epoch: 0,
            });
            let fresh = entry.streams == 0 && entry.epoch == 0;
            let escalated = controlling && !entry.controlling;
            entry.streams += 1;
            entry.epoch += 1;
            entry.controlling |= controlling;
            (fresh || escalated).then_some(entry.controlling)
        };
        if let Some(controlling) = announce {
            self.presence().engaged(&Engagement {
                peer,
                session: session.to_owned(),
                controlling,
            });
        }
        Hold {
            engagements: Some(Arc::clone(self)),
            key,
        }
    }

    fn leave(self: &Arc<Self>, key: &Key) {
        let epoch = {
            let mut live = self.lock();
            let Some(entry) = live.get_mut(key) else {
                return;
            };
            entry.streams -= 1;
            if entry.streams > 0 {
                return;
            }
            entry.epoch += 1;
            entry.epoch
        };
        if self.linger.is_zero() {
            self.release(key, epoch);
            return;
        }
        let (engagements, owned) = (Arc::clone(self), key.clone());
        let started = thread::Builder::new()
            .name("bohio-gui-presence".to_owned())
            .spawn(move || {
                thread::sleep(engagements.linger);
                engagements.release(&owned, epoch);
            });
        if started.is_err() {
            self.release(key, epoch);
        }
    }

    /// Ends the engagement when nothing happened to it since `epoch`.
    fn release(&self, key: &Key, epoch: u64) {
        let ended = {
            let mut live = self.lock();
            match live.get(key) {
                Some(entry) if entry.streams == 0 && entry.epoch == epoch => live.remove(key),
                _ => None,
            }
        };
        if let Some(entry) = ended {
            self.presence().released(&Engagement {
                peer: key.0,
                session: key.1.clone(),
                controlling: entry.controlling,
            });
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(engagements) = &self.engagements {
            engagements.leave(&self.key);
        }
    }
}
