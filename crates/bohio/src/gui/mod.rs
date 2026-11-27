//! `bohio-gui/1`: a peer's view of this host's desktop, over a [`Desktop`].
//!
//! The engine serves one stream per view. It decides each request from the peer's grant as it is then, bounds what a
//! peer may ask (the rates, the sizes, the clipboard), tells the person at the screen ([`presence`]) and asks them first
//! when `[gui] approval` says so. What reaches the desktop is a request that passed all of that, one call at a time.
//!
//! A node serves the desktop of its default desktop source ([`crate::access::Sources`]): the platform's, an
//! application's own (`Node::with_desktop`) or any provider's. Everything here is tested against an in-memory one, with
//! no screen.

#![forbid(unsafe_code)]

#[cfg(feature = "gui")]
mod adapt;
mod encode;
pub mod helper;
pub mod helper_exec;
pub mod helper_upload;
mod limits;
#[cfg(feature = "notify")]
mod notify;
pub mod presence;
mod settle;
#[cfg(feature = "gui")]
mod stream;
/// Without the `gui` feature a watch is `unsupported`: there are no encoders for its rectangles.
#[cfg(not(feature = "gui"))]
mod stream {
    use super::{Duration, ErrorBody, ErrorCode, Reply, Stream};

    pub(super) struct Watching;

    impl Stream<'_> {
        pub(super) fn watch(&mut self, _: &bohio_proto::gui::Watch) -> Reply {
            Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "this build of the host has no image encoders",
            ))
        }

        pub(super) fn unwatch(&mut self) -> Reply {
            Ok(())
        }

        pub(super) fn ack(&mut self, _: &bohio_proto::gui::Ack) -> Reply {
            Ok(())
        }

        pub(super) fn wake_in(&self, poll: Duration) -> Duration {
            poll
        }

        pub(super) fn tick(&mut self) -> bool {
            true
        }
    }
}

#[cfg(target_os = "macos")]
pub use crate::platform::gui::{MacosDesktop, MacosSettings};
#[cfg(target_os = "linux")]
pub use crate::platform::gui::{
    OwnServer, OwnSession, RuntimeDirError, check_runtime_dir, own_display, own_runtime_dir,
    own_uid,
};
#[cfg(all(target_os = "linux", feature = "gui-wayland"))]
pub use crate::platform::gui::{Persist, WaylandDesktop, WaylandSettings};
#[cfg(windows)]
pub use crate::platform::gui::{WindowsDesktop, WindowsSettings};
pub use bohio_x11::{Capabilities, Desktop, DesktopState, Frame, GuiError};
#[cfg(target_os = "linux")]
pub use bohio_x11::{X11Desktop, X11Settings};
#[cfg(feature = "notify")]
pub use notify::DesktopNotifier;
pub use presence::{
    Approval, ApprovalRequest, Approvals, Decide, Engagement, LocalPresence, LogNotifier, Notice,
    Notifier, Presence,
};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use bohio_proto::gui::{
    ClipboardGet, ClipboardSet, ClipboardText, Display, GuiInfo, GuiInput, GuiOutput, GuiRequest,
    GuiSession, ImageFormat, ImageHeader, Input, InputDone, InputEvent, Point, Ready, RequestError,
    Settle, SettleOptions, Settled, SettledFrame, Snapshot, State, Want, is_key,
};
use bohio_proto::{ErrorBody, ErrorCode};
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure};

use crate::access::{KindDesktop, Sources};
use crate::config::{ApprovalMode, GuiConfig, Indicator};
use crate::control::Current;
use crate::limit::{Counted, Slots};
use crate::link::{GuiChange, NodeEvent, NodeEvents};
use crate::policy::{Grant, Permission};
use crate::stream::{chunk_size, send_waiting};
use limits::{InputRate, PerPeerRate};
use presence::{Engagements, NotifierCell};

/// What the person allowed per peer and session, with when.
type Allowed = HashMap<(String, String), Vec<(Want, Instant)>>;

/// What a node holds for its desktop: the desktop itself, the bounds shared by every stream, the engagements and the
/// approvals.
pub struct GuiState {
    sources: Arc<Sources>,
    /// One call into the desktop at a time: platform input and capture are not concurrent.
    io: Mutex<()>,
    slots: Slots,
    per_peer: Counted<String>,
    snapshots: PerPeerRate,
    /// How often a peer may make the person at the desk decide, a minute.
    asks: PerPeerRate,
    /// When a refused ask was last logged, so a peer that keeps asking writes one line a minute.
    asks_logged: Mutex<Option<Instant>>,
    engagements: Arc<Engagements>,
    approvals: Arc<Approvals>,
    notifier: Arc<NotifierCell>,
    /// What the person allowed, per peer and session, and when: a later open is not asked again only for what it already
    /// allowed.
    allowed: Mutex<Allowed>,
    helpers: helper::Helpers,
}

impl core::fmt::Debug for GuiState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GuiState").finish_non_exhaustive()
    }
}

/// The only desktop session a node serves today.
pub const DEFAULT_SESSION: &str = "default";

impl GuiState {
    /// The state of a node whose settings are `config`: no desktop yet, [`LocalPresence`] for its presence.
    pub fn new(config: &GuiConfig, sources: Arc<Sources>) -> Arc<Self> {
        if config.input_when_lock_unknown {
            tracing::warn!(
                "non-default: [gui] input_when_lock_unknown is true: input is applied to a desktop that cannot say whether its screen is locked, so a lock screen may be typed at"
            );
        }
        let notifier = Arc::new(NotifierCell::default());
        let approvals = Arc::new(Approvals::default());
        let presence: Arc<dyn Presence> = Arc::new(LocalPresence::new(
            Arc::clone(&notifier),
            Arc::clone(&approvals),
        ));
        Arc::new(Self {
            sources,
            io: Mutex::new(()),
            slots: Slots::new(config.max_streams),
            per_peer: Counted::new(config.max_streams_per_peer),
            snapshots: PerPeerRate::new(config.snapshots_per_minute),
            asks: PerPeerRate::new(config.approval_asks_per_minute),
            asks_logged: Mutex::new(None),
            engagements: Engagements::new(
                presence,
                config.indicator_linger.0,
                config.indicator == Indicator::Notify,
            ),
            approvals,
            notifier,
            allowed: Mutex::new(HashMap::new()),
            helpers: helper::Helpers::new(config.max_helpers),
        })
    }

    /// Replaces [`LocalPresence`] with an application's own.
    pub fn set_presence(&self, presence: Arc<dyn Presence>) {
        self.engagements.set_presence(presence);
    }

    /// Where [`LocalPresence`] shows its notices.
    pub fn set_notifier(&self, notifier: Arc<dyn Notifier>) {
        self.notifier.set(notifier);
    }

    /// The approvals [`LocalPresence`] waits on, which the node's local endpoint lists and decides.
    pub fn approvals(&self) -> Arc<Approvals> {
        Arc::clone(&self.approvals)
    }

    /// The node's sources.
    pub(crate) fn sources(&self) -> Arc<Sources> {
        Arc::clone(&self.sources)
    }

    /// A session on the default desktop source: what a stream drives.
    fn desktop(&self, name: Option<&str>, grant: &Grant) -> Result<Arc<dyn Desktop>, ErrorBody> {
        let source = self.sources.desktop(name)?;
        if !self.sources.permits(grant, source.name()) {
            return Err(denied("this peer may not use that desktop source"));
        }
        let session = KindDesktop::open(source)?;
        Ok(Arc::new(session))
    }

    /// Logs that `peer` asked past `approval_asks_per_minute`, at most once a minute.
    fn log_ask_refused(&self, peer: PeerId) {
        let mut logged = self
            .asks_logged
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if logged.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(60)) {
            *logged = Some(now);
            tracing::warn!(%peer, "a peer asked the person at the desk more often than [gui] approval_asks_per_minute allows: the open is denied without asking");
        }
    }

    fn call<R>(&self, call: impl FnOnce() -> R) -> R {
        let _turn = self.io.lock().unwrap_or_else(PoisonError::into_inner);
        call()
    }

    /// Whether the person already allowed every item of `wants` to this peer on this session within `ttl`.
    fn allowed_within(&self, peer: &PeerId, session: &str, wants: &[Want], ttl: Duration) -> bool {
        let mut allowed = self.allowed.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        allowed.retain(|_, items| {
            items.retain(|(_, at)| now.duration_since(*at) < ttl);
            !items.is_empty()
        });
        allowed
            .get(&(peer.to_string(), session.to_owned()))
            .is_some_and(|items| {
                wants
                    .iter()
                    .all(|want| items.iter().any(|(item, _)| item == want))
            })
    }

    fn remember(&self, peer: &PeerId, session: &str, wants: &[Want]) {
        let now = Instant::now();
        let mut allowed = self.allowed.lock().unwrap_or_else(PoisonError::into_inner);
        let items = allowed
            .entry((peer.to_string(), session.to_owned()))
            .or_default();
        for want in wants {
            items.retain(|(item, _)| item != want);
            items.push((*want, now));
        }
    }

    /// What `info` says of the desktop to a peer with `grant`: the sessions only when it holds `gui.view` or
    /// `gui.input`. Asked at every `info`, never cached.
    pub fn info(&self, config: &GuiConfig, grant: &Grant) -> GuiInfo {
        let mut info = self.in_process_info(config, grant);
        if config.enabled
            && (grant.allows(Permission::GuiView) || grant.allows(Permission::GuiInput))
        {
            let helpers = self
                .helpers
                .sessions(|entry| entry.reachable(&self.sources, grant));
            if !helpers.is_empty() {
                info.available = true;
                info.reason = None;
                info.sessions.extend(helpers);
            }
        }
        info
    }

    /// Whether the node's own desktop answers now.
    fn in_process_available(&self) -> bool {
        self.sources.desktop(None).ok().is_some_and(|source| {
            KindDesktop::open(source).is_ok_and(|desktop| self.call(|| desktop.state()).is_ok())
        })
    }

    fn in_process_info(&self, config: &GuiConfig, grant: &Grant) -> GuiInfo {
        let formats: Vec<ImageFormat> = encode::available()
            .into_iter()
            .filter(|format| config.formats.contains(format))
            .collect();
        let mut info = GuiInfo {
            enabled: config.enabled,
            formats,
            ..GuiInfo::default()
        };
        if !config.enabled {
            info.reason = Some("this host does not serve its desktop ([gui] enabled)".to_owned());
            return info;
        }
        // What a desktop says of itself is for a peer that may use it: asking would also show a portal's dialog, and the answer
        // names binaries and units.
        if !(grant.allows(Permission::GuiView) || grant.allows(Permission::GuiInput)) {
            return info;
        }
        let source = match self.sources.desktop(None) {
            Ok(source) => source,
            Err(error) => {
                info.reason = Some(error.message);
                return info;
            }
        };
        // Asked now, never cached: a source can lose its screen while the node runs.
        let described = source.get().describe();
        if let Ok(described) = &described {
            info.backend = Some(described.provider.clone());
            info.input = !described.lacks(bohio_api::vocab::desktop::INPUT);
            info.clipboard = !described.lacks(bohio_api::vocab::desktop::CLIPBOARD);
            info.lock_detection = !described.lacks(bohio_api::vocab::desktop::LOCK_DETECTION);
        }
        let opened = KindDesktop::open(source);
        let desktop = match opened {
            Ok(desktop) => desktop,
            Err(error) => {
                info.reason = Some(error.message);
                return info;
            }
        };
        match self.call(|| desktop.state()) {
            Ok(state) => {
                info.available = true;
                if grant.allows(Permission::GuiView) || grant.allows(Permission::GuiInput) {
                    info.sessions.push(GuiSession {
                        id: DEFAULT_SESSION.to_owned(),
                        user: state.user,
                        active: state.active,
                        locked: state.locked,
                        displays: state.displays,
                    });
                }
            }
            Err(error) => info.reason = Some(error.message),
        }
        info
    }
}

/// What [`serve`] needs besides the stream.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    /// The peer's grant when the stream opened.
    pub grant: &'a Grant,
    /// Where the grant is read again before each request, so a grant narrowed while the stream is open applies to its
    /// next request; `grant` is used when absent.
    pub current: Option<Current<'a>>,
    /// `[gui]`.
    pub config: &'a GuiConfig,
    /// The node's desktop, bounds and presence.
    pub state: &'a GuiState,
    /// The peer.
    pub peer: PeerId,
    /// Where the owner hears what happened.
    pub events: &'a dyn NodeEvents,
    /// The session the stream serves, as a request names it: [`DEFAULT_SESSION`] for the node's own desktop, a helper's
    /// listed id for a helper's.
    pub session: &'a str,
}

/// What a stream did, for the owner's event and the caller's log. Counts only: never pixels, typed text, key names or
/// clipboard content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ended {
    /// Snapshots sent.
    pub snapshots: u64,
    /// Screen updates sent by a watch.
    pub frames: u64,
    /// Input events applied.
    pub input_events: u64,
    /// Clipboard reads.
    pub clipboard_reads: u64,
    /// Clipboard writes.
    pub clipboard_writes: u64,
}

/// Whether `grant` holds what `want` needs: `gui.view`, `gui.input`, or `gui.clipboard` beside one of them.
fn permits(grant: &Grant, want: Want) -> bool {
    match want {
        Want::View => grant.allows(Permission::GuiView),
        Want::Input => grant.allows(Permission::GuiInput),
        Want::Clipboard => {
            grant.allows(Permission::GuiClipboard)
                && (grant.allows(Permission::GuiView) || grant.allows(Permission::GuiInput))
        }
    }
}

/// Whether `grant` still covers what the stream's open asked for; a request that cannot be read is covered, so the engine
/// answers it.
pub fn covers(grant: &Grant, metadata: &[u8]) -> bool {
    GuiRequest::from_metadata(metadata).map_or(true, |request| {
        request.wants().into_iter().all(|want| permits(grant, want))
            && request
                .source
                .as_deref()
                .is_none_or(|source| grant.permits_source(source))
            && request
                .session
                .as_deref()
                .is_none_or(|session| exec_session_permitted(grant, session))
    })
}

/// Whether `grant` reaches the exec source behind the helper session `session` (`exec:SOURCE:NAME`); any other session is
/// not behind a source. A source name can hold a colon, so a grant naming any leading part of the text covers it: the open
/// has already checked the exact source, and this only ends a stream whose grant was narrowed.
fn exec_session_permitted(grant: &Grant, session: &str) -> bool {
    let Some(rest) = session.strip_prefix("exec:") else {
        return true;
    };
    rest.match_indices(':')
        .map(|(at, _)| &rest[..at])
        .chain(std::iter::once(rest))
        .any(|name| grant.permits_source(name))
}

fn denied(message: &str) -> ErrorBody {
    ErrorBody::new(ErrorCode::Denied, message)
}

fn bad(message: &str) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

/// Serves one `bohio-gui/1` stream whose open carried `metadata`, until the client closes it.
///
/// Order of checks at the open: decode (`bad_request`, and no `want` is `bad_request`), `[gui] enabled` and a desktop
/// (`unsupported`), each wanted item against the grant (`denied`), the session (`not_found`), a free slot
/// (`busy`), the desktop's state, the person at the desk when `approval = "local"` (`denied`, "not approved"). A refused
/// open gets one `error` frame, the same error is returned, and the stream stays open for the caller to close.
///
/// # Errors
///
/// The [`ErrorBody`] sent as the stream's `error` frame.
pub fn serve(
    channel: Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<Ended, ErrorBody> {
    match open(&channel, metadata, context) {
        Ok(ended) => Ok(ended),
        Err(error) => Err(refuse(channel.as_ref(), error, context)),
    }
}

/// As [`serve`], for a node: a request that names a helper's session goes to that helper, and so does one that names none when
/// the node has no desktop of its own and a helper has (see [`helper`]); everything else is the node's own desktop. The stream
/// is left open for the caller to close.
pub fn serve_chosen(channel: &Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) {
    match context
        .state
        .choose(context.config, context.grant, metadata)
    {
        helper::Chosen::InProcess => match serve(Arc::clone(channel), metadata, context) {
            Ok(ended) => tracing::debug!(snapshots = ended.snapshots, "gui ended"),
            Err(error) => tracing::info!(code = ?error.code, "gui refused"),
        },
        helper::Chosen::Helper(entry) => {
            let relayed = Context {
                session: &entry.id,
                ..*context
            };
            match helper::serve_relayed(channel, &entry, metadata, &relayed) {
                Ok(()) => tracing::debug!("gui ended"),
                Err(error) => tracing::info!(code = ?error.code, "gui refused"),
            }
        }
        helper::Chosen::Refuse(error) => {
            let error = refuse(channel.as_ref(), error, context);
            tracing::info!(code = ?error.code, "gui refused");
        }
    }
}

/// Sends `error` as the stream's last frame and tells the owner an open was refused.
fn refuse(channel: &dyn Channel, error: ErrorBody, context: &Context<'_>) -> ErrorBody {
    let poll = context.config.poll_interval.0;
    let frame = GuiOutput::Error(error.clone())
        .encode()
        .unwrap_or_else(|_| vec![0x05]);
    let _ = send_waiting(channel, &frame, poll, poll);
    context.events.event(&NodeEvent::Gui {
        peer: context.peer,
        session: context.session.to_owned(),
        change: GuiChange::Refused {
            reason: error.message.clone(),
        },
    });
    error
}

fn busy() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Busy,
        "the host is serving as many desktop streams as it allows",
    )
}

fn open(
    channel: &Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<Ended, ErrorBody> {
    let config = context.config;
    let request =
        GuiRequest::from_metadata(metadata).map_err(|_| bad("the request cannot be read"))?;
    if request.want.as_ref().is_some_and(Vec::is_empty) {
        return Err(bad("name at least one thing to want"));
    }
    if !config.enabled {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "this host does not serve its desktop",
        ));
    }
    let desktop = context
        .state
        .desktop(request.source.as_deref(), context.grant)?;
    let wants = request.wants();
    if let Some(missing) = wants.iter().find(|want| !permits(context.grant, **want)) {
        return Err(denied(&format!("this peer may not {missing} the desktop")));
    }
    if request
        .session
        .as_deref()
        .is_some_and(|session| session != context.session)
    {
        return Err(ErrorBody::new(ErrorCode::NotFound, "no such session"));
    }
    let _slot = context.state.slots.acquire().ok_or_else(busy)?;
    let _mine = context
        .state
        .per_peer
        .take(context.peer.to_string())
        .ok_or_else(busy)?;
    let state = context
        .state
        .call(|| desktop.state())
        .map_err(ErrorBody::from)?;
    approve(channel.as_ref(), context, &wants)?;
    let controlling = wants.contains(&Want::Input);
    let _engaged = context
        .state
        .engagements
        .enter(context.peer, context.session, controlling);
    let mut stream = Stream {
        channel: channel.as_ref(),
        context,
        desktop: desktop.as_ref(),
        granted: wants.clone(),
        counts: Ended::default(),
        rate: InputRate::new(config.max_input_rate),
        last_state: state.wire(),
        chunk: chunk_size(channel.as_ref(), config.chunk_size),
        watch: None,
    };
    let ready = GuiOutput::Ready(Ready {
        session: context.session.to_owned(),
        granted: wants.clone(),
        state: state.wire(),
    });
    if !stream.send(&ready) {
        return Err(ErrorBody::new(
            ErrorCode::Io,
            "the stream ended before it opened",
        ));
    }
    context.events.event(&NodeEvent::Gui {
        peer: context.peer,
        session: context.session.to_owned(),
        change: GuiChange::Started { granted: wants },
    });
    stream.run();
    let ended = stream.counts;
    context.events.event(&NodeEvent::Gui {
        peer: context.peer,
        session: context.session.to_owned(),
        change: GuiChange::Ended {
            snapshots: ended.snapshots,
            frames: ended.frames,
            input_events: ended.input_events,
            clipboard_reads: ended.clipboard_reads,
            clipboard_writes: ended.clipboard_writes,
            reason: "the stream ended".to_owned(),
        },
    });
    Ok(ended)
}

/// Asks the person at the desk, when the host is set to, unless they allowed this peer on this session recently.
fn approve(channel: &dyn Channel, context: &Context<'_>, wants: &[Want]) -> Result<(), ErrorBody> {
    let config = context.config;
    if config.approval != ApprovalMode::Local {
        return Ok(());
    }
    let state = context.state;
    if state.allowed_within(&context.peer, context.session, wants, config.approval_ttl.0) {
        return Ok(());
    }
    if !state.asks.admit(&context.peer.to_string()) {
        state.log_ask_refused(context.peer);
        return Err(denied("not approved"));
    }
    let cancelled = AtomicBool::new(false);
    let request = ApprovalRequest {
        peer: context.peer,
        session: context.session,
        want: wants,
        since: SystemTime::now(),
        timeout: config.approval_timeout.0,
        poll: config.poll_interval.0,
        cancelled: &cancelled,
    };
    let finished = AtomicBool::new(false);
    let decision = thread::scope(|scope| {
        scope.spawn(|| {
            while !finished.load(Ordering::SeqCst) {
                if channel.is_closed() {
                    cancelled.store(true, Ordering::SeqCst);
                    return;
                }
                thread::sleep(config.poll_interval.0);
            }
        });
        let decision = state.engagements.presence().approve(&request);
        finished.store(true, Ordering::SeqCst);
        decision
    });
    match decision {
        Approval::Allow => {
            state.remember(&context.peer, context.session, wants);
            Ok(())
        }
        Approval::Deny => Err(denied("not approved")),
    }
}

struct Stream<'a> {
    channel: &'a dyn Channel,
    context: &'a Context<'a>,
    desktop: &'a dyn Desktop,
    /// What the stream was opened for, each item checked against the grant again at every request.
    granted: Vec<Want>,
    counts: Ended,
    rate: InputRate,
    last_state: State,
    chunk: usize,
    /// The screen being watched, when a `watch` is running.
    #[cfg_attr(
        not(feature = "gui"),
        allow(dead_code, reason = "a build without images never starts a watch")
    )]
    watch: Option<stream::Watching>,
}

type Reply = Result<(), ErrorBody>;

impl Stream<'_> {
    fn config(&self) -> &GuiConfig {
        self.context.config
    }

    fn send(&self, output: &GuiOutput) -> bool {
        let config = self.config();
        let frame = output.encode().unwrap_or_else(|_| vec![0x05]);
        send_waiting(
            self.channel,
            &frame,
            config.poll_interval.0,
            config.idle_timeout.0,
        )
    }

    fn call<R>(&self, call: impl FnOnce() -> R) -> R {
        self.context.state.call(call)
    }

    fn state(&self) -> Result<DesktopState, GuiError> {
        self.call(|| self.desktop.state())
    }

    fn run(&mut self) {
        let config = self.config();
        let (poll, idle, interval) = (
            config.poll_interval.0,
            config.idle_timeout.0,
            config.state_interval.0,
        );
        let mut last_request = Instant::now();
        let mut next_state = Instant::now() + interval;
        loop {
            match self.channel.receive(Some(self.wake_in(poll))) {
                Ok(Some(frame)) => {
                    last_request = Instant::now();
                    if !self.handle(&frame) {
                        return;
                    }
                }
                Err(Failure::Timeout) => {
                    if self.channel.is_closed() || last_request.elapsed() >= idle {
                        return;
                    }
                }
                Ok(None) | Err(_) => return,
            }
            if !self.tick() {
                return;
            }
            if Instant::now() >= next_state {
                next_state = Instant::now() + interval;
                if !self.poll_state() {
                    return;
                }
            }
        }
    }

    /// Sends a `state` frame when the desktop's state changed. `false` when the stream ended.
    fn poll_state(&mut self) -> bool {
        let Ok(now) = self.state() else {
            return true;
        };
        let wire = now.wire();
        if wire == self.last_state {
            return true;
        }
        self.last_state = wire.clone();
        self.send(&GuiOutput::State(wire))
    }

    /// Answers one frame. `false` when the stream ended.
    fn handle(&mut self, frame: &[u8]) -> bool {
        let input = match GuiInput::decode(frame) {
            Ok(input) => input,
            // A frame this version does not know is skipped.
            Err(bohio_proto::Error::Tag(_)) => return true,
            Err(_) => {
                return self.send(&GuiOutput::RequestError(RequestError::new(
                    0,
                    bad("the request cannot be read"),
                )));
            }
        };
        let (seq, result) = match input {
            GuiInput::Snapshot(request) => (request.seq, self.snapshot(&request)),
            GuiInput::Input(request) => (request.seq, self.input(&request)),
            GuiInput::Settle(request) => (request.seq, self.settle(&request)),
            GuiInput::Watch(request) => (request.seq, self.watch(&request)),
            GuiInput::Unwatch(request) => (request.seq, self.unwatch()),
            GuiInput::Ack(request) => (0, self.ack(&request)),
            GuiInput::ClipboardGet(request) => (request.seq, self.clipboard_get(&request)),
            GuiInput::ClipboardSet(request) => (request.seq, self.clipboard_set(&request)),
            _ => return true,
        };
        match result {
            Ok(()) => !self.channel.is_closed(),
            Err(body) => self.send(&GuiOutput::RequestError(RequestError::new(seq, body))),
        }
    }

    /// Checks that the stream may do what `want` is for now: it was opened for it and the peer's grant still holds it.
    fn need(&self, want: Want) -> Result<(), ErrorBody> {
        if !self.granted.contains(&want) {
            return Err(denied(&format!("this stream was not opened to {want}")));
        }
        let now = match self.context.current {
            Some(current) => (current.0)()?,
            None => self.context.grant.clone(),
        };
        if permits(&now, want) {
            Ok(())
        } else {
            Err(denied(&format!(
                "this peer may no longer {want} the desktop"
            )))
        }
    }

    /// The display and the region of a request, inside the display.
    fn target(
        &self,
        state: &DesktopState,
        display: Option<&bohio_proto::gui::DisplayId>,
        region: Option<bohio_proto::gui::Rect>,
    ) -> Result<(Display, bohio_proto::gui::Rect), ErrorBody> {
        let display = state.display(display)?.clone();
        let whole = bohio_proto::gui::Rect::new(0, 0, display.width, display.height);
        let region = region.unwrap_or(whole);
        if !region.fits(display.width, display.height) {
            return Err(bad("the region is not inside the display"));
        }
        if region.area() > self.config().max_snapshot_pixels {
            return Err(bad("the region has more pixels than this host captures"));
        }
        Ok((display, region))
    }

    fn settle_params(
        &self,
        quiet_ms: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> Result<settle::Params, ErrorBody> {
        let config = self.config();
        let time = |ms: Option<u64>, default: Duration, what: &str| match ms {
            Some(0) => Err(bad(&format!("{what} must not be zero"))),
            Some(ms) if Duration::from_millis(ms) > config.max_settle_wait.0 => Err(bad(&format!(
                "{what} is longer than the {:?} this host waits ([gui] max_settle_wait)",
                config.max_settle_wait.0
            ))),
            Some(ms) => Ok(Duration::from_millis(ms)),
            None => Ok(default),
        };
        Ok(settle::Params {
            interval: config.settle_interval.0,
            quiet: time(quiet_ms, config.settle_quiet.0, "quiet_ms")?,
            timeout: time(timeout_ms, config.settle_timeout.0, "timeout_ms")?,
            ignore_tiles: config.settle_ignore_tiles,
        })
    }

    fn admit_snapshot(&self) -> Result<(), ErrorBody> {
        if self
            .context
            .state
            .snapshots
            .admit(&self.context.peer.to_string())
        {
            Ok(())
        } else {
            Err(ErrorBody::new(
                ErrorCode::Busy,
                "this peer has taken as many snapshots a minute as the host allows",
            ))
        }
    }

    fn snapshot(&mut self, request: &Snapshot) -> Reply {
        self.need(Want::View)?;
        let config = self.config();
        let state = self.state()?;
        let (display, region) = self.target(&state, request.display.as_ref(), request.region)?;
        let format = request
            .format
            .or_else(|| config.formats.first().copied())
            .ok_or_else(|| bad("no image format"))?;
        if !config.formats.contains(&format) {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!("this host does not offer {format}"),
            ));
        }
        let quality = request.quality.unwrap_or(config.jpeg_quality);
        if !(1..=100).contains(&quality) {
            return Err(bad("the quality is from 1 to 100"));
        }
        if request.max_width == Some(0) || request.max_height == Some(0) {
            return Err(bad("a maximum size must not be zero"));
        }
        let settle_params = request
            .settle
            .map(|options: SettleOptions| self.settle_params(options.quiet_ms, options.timeout_ms))
            .transpose()?;
        self.admit_snapshot()?;
        let asked = (request.region.is_some()).then_some(region);
        let mut settled = None;
        let frame = if let Some(params) = settle_params {
            let mut capture = || self.call(|| self.desktop.capture(&display.id, asked));
            let outcome = settle::settle(&mut capture, &params, &|| self.channel.is_closed())?;
            settled = Some(Settled {
                settled: outcome.settled,
                waited_ms: millis(outcome.waited),
            });
            outcome.frame
        } else {
            self.call(|| self.desktop.capture(&display.id, asked))?
        };
        if (frame.width(), frame.height()) != (region.width, region.height) {
            return Err(ErrorBody::new(
                ErrorCode::Internal,
                "the desktop returned a picture of another size",
            ));
        }
        let size = encode::fitted(
            frame.width(),
            frame.height(),
            request.max_width,
            request.max_height,
        );
        let bytes = encode::encode(frame, size, format, quality)?;
        let cursor = if request.cursor {
            self.call(|| self.desktop.cursor())
                .filter(|(on, _, _)| *on == display.id)
                .map(|(_, x, y)| Point { x, y })
        } else {
            None
        };
        let header = ImageHeader {
            seq: request.seq,
            display: display.id.clone(),
            format,
            width: size.0,
            height: size.1,
            region,
            scale: f64::from(size.0) / f64::from(region.width),
            cursor,
            length: bytes.len() as u64,
            settled,
        };
        if !self.send(&GuiOutput::Image(header)) {
            return Err(ended());
        }
        for piece in bytes.chunks(self.chunk) {
            if !self.send(&GuiOutput::Data(piece.to_vec())) {
                return Err(ended());
            }
        }
        self.counts.snapshots += 1;
        Ok(())
    }

    fn settle(&mut self, request: &Settle) -> Reply {
        self.need(Want::View)?;
        let state = self.state()?;
        let (display, region) = self.target(&state, request.display.as_ref(), request.region)?;
        let params = self.settle_params(request.quiet_ms, request.timeout_ms)?;
        self.admit_snapshot()?;
        let asked = request.region.map(|_| region);
        let mut capture = || self.call(|| self.desktop.capture(&display.id, asked));
        let outcome = settle::settle(&mut capture, &params, &|| self.channel.is_closed())?;
        let frame = GuiOutput::Settled(SettledFrame {
            seq: request.seq,
            settled: outcome.settled,
            waited_ms: millis(outcome.waited),
        });
        if self.send(&frame) {
            Ok(())
        } else {
            Err(ended())
        }
    }

    fn input(&mut self, request: &Input) -> Reply {
        self.need(Want::Input)?;
        let config = self.config();
        if request.events.is_empty() {
            return Err(bad("an input frame holds at least one event"));
        }
        if request.events.len() > config.max_batch_events {
            return Err(bad("the frame holds more events than this host takes"));
        }
        let state = self.state()?;
        let mut cost = 0_u32;
        let mut waits = Duration::ZERO;
        for event in &request.events {
            cost = cost.saturating_add(match event {
                InputEvent::Key { key, .. } => {
                    if !is_key(key) {
                        return Err(bad("that is not a key name"));
                    }
                    1
                }
                InputEvent::Text { text } => {
                    let characters = text.chars().count();
                    if characters > config.max_text {
                        return Err(bad("the text is longer than this host types at once"));
                    }
                    // A key press and release for each character: the cost is what the desktop is held for.
                    u32::try_from(characters).unwrap_or(u32::MAX).max(1)
                }
                InputEvent::Wait { ms } => {
                    waits = waits.saturating_add(Duration::from_millis(*ms));
                    1
                }
                InputEvent::Button { count: Some(0), .. } => {
                    return Err(bad("a click count is at least 1"));
                }
                InputEvent::Button { count, display, .. } => {
                    state.display(display.as_ref())?;
                    count.unwrap_or(1)
                }
                InputEvent::Move { display, .. } => {
                    state.display(display.as_ref())?;
                    1
                }
                InputEvent::Scroll {
                    display, dx, dy, ..
                } => {
                    state.display(display.as_ref())?;
                    dx.unsigned_abs().saturating_add(dy.unsigned_abs()).max(1)
                }
            });
        }
        if waits > config.max_batch_wait.0 {
            return Err(bad(
                "the waits of the frame add up to more than this host allows",
            ));
        }
        if !state.active {
            return Err(ErrorBody::new(
                ErrorCode::Unavailable,
                "the desktop is not at the screen",
            ));
        }
        if state.locked == Some(true) && !config.input_when_locked {
            return Err(ErrorBody::new(
                ErrorCode::Unavailable,
                "the screen is locked",
            ));
        }
        if state.locked.is_none() && !config.input_when_lock_unknown {
            return Err(ErrorBody::new(
                ErrorCode::Unavailable,
                "the lock state is not known",
            ));
        }
        if !self.rate.admit(cost, Instant::now()) {
            return Err(ErrorBody::new(
                ErrorCode::Busy,
                "this stream sends more input a second than the host allows",
            ));
        }
        let applied = self.apply(&request.events)?;
        self.counts.input_events += u64::from(applied);
        let done = GuiOutput::InputDone(InputDone {
            seq: request.seq,
            applied,
        });
        if self.send(&done) {
            Ok(())
        } else {
            Err(ended())
        }
    }

    /// Applies the events in order: the runs between `wait` events go to the desktop, a wait pauses here.
    fn apply(&self, events: &[InputEvent]) -> Result<u32, ErrorBody> {
        let mut applied = 0_u32;
        let mut run: &[InputEvent] = events;
        while !run.is_empty() {
            let wait = run
                .iter()
                .position(|event| matches!(event, InputEvent::Wait { .. }));
            let (now, rest) = run.split_at(wait.unwrap_or(run.len()));
            if !now.is_empty() {
                let took = self.call(|| self.desktop.input(now))?;
                applied = applied.saturating_add(took);
                if (took as usize) < now.len() {
                    return Ok(applied);
                }
            }
            match rest.split_first() {
                Some((InputEvent::Wait { ms }, after)) => {
                    self.pause(Duration::from_millis(*ms));
                    applied = applied.saturating_add(1);
                    run = after;
                }
                _ => break,
            }
        }
        Ok(applied)
    }

    /// Waits `time`, looking at the stream so that one that closed ends the wait.
    fn pause(&self, time: Duration) {
        let until = Instant::now() + time;
        let poll = self.config().poll_interval.0;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() || self.channel.is_closed() {
                return;
            }
            thread::sleep(left.min(poll));
        }
    }

    fn clipboard_get(&mut self, request: &ClipboardGet) -> Reply {
        self.need(Want::Clipboard)?;
        let text = self.call(|| self.desktop.clipboard_text())?;
        if text.len() > self.config().clipboard_max_bytes {
            return Err(ErrorBody::new(
                ErrorCode::Io,
                "the clipboard holds more than this host sends",
            ));
        }
        self.counts.clipboard_reads += 1;
        self.context.events.event(&NodeEvent::Gui {
            peer: self.context.peer,
            session: self.context.session.to_owned(),
            change: GuiChange::ClipboardRead,
        });
        let frame = GuiOutput::Clipboard(ClipboardText {
            seq: request.seq,
            text,
        });
        if self.send(&frame) {
            Ok(())
        } else {
            Err(ended())
        }
    }

    fn clipboard_set(&mut self, request: &ClipboardSet) -> Reply {
        self.need(Want::Clipboard)?;
        if request.text.len() > self.config().clipboard_max_bytes {
            return Err(bad("the text is longer than this host takes"));
        }
        self.call(|| self.desktop.set_clipboard_text(&request.text))?;
        self.counts.clipboard_writes += 1;
        self.context.events.event(&NodeEvent::Gui {
            peer: self.context.peer,
            session: self.context.session.to_owned(),
            change: GuiChange::ClipboardWritten,
        });
        let frame = GuiOutput::Clipboard(ClipboardText {
            seq: request.seq,
            text: String::new(),
        });
        if self.send(&frame) {
            Ok(())
        } else {
            Err(ended())
        }
    }
}

fn ended() -> ErrorBody {
    ErrorBody::new(ErrorCode::Io, "the stream ended before the answer was sent")
}

fn millis(time: Duration) -> u64 {
    u64::try_from(time.as_millis()).unwrap_or(u64::MAX)
}
