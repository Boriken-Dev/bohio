//! The per-user GUI helper: how a node that runs as a service (a session with no desktop) serves the desktop of a user who
//! lends it.
//!
//! The node listens on the helper endpoint and serves only the accounts `[gui] helper_accounts` lists: who connected is the
//! operating system's answer about the connection, checked before a byte is read. A helper connects out, says which session it
//! serves (`bohio-gui-helper/1`), and answers the `bohio-gui/1` streams the node opens on the same connection, one per peer
//! stream, each carrying the peer's request and what the node decided for it. The helper holds no key and reads no peer table.
//!
//! The helper checks the other way too: before it sends anything it asks who serves the endpoint, and refuses any account but the
//! node's. Both checks are of the account, not of anything the other side says.

#![forbid(unsafe_code)]

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use bohio_client::connect::{Opened, Opener};
use bohio_client::dial::Endpoint;
use bohio_client::link::Settings;
use bohio_client::relay::relay;
use bohio_proto::gui::{
    GuiInput, GuiOutput, GuiRequest, GuiSession, HelperChange, HelperHello, HelperMessage,
    HelperOpen, HelperPolicy, RequestError, State, Want,
};
use bohio_proto::local::GUI_HELPER;
use bohio_proto::{ErrorBody, ErrorCode, protocol};
use cabuya::link::{Accepted, Host, Options, Services, serve_with};
use cabuya::mux::{CloseReason, LinkStream, Mux, Side, StreamLife};
use cabuya::peers::{NoEvents, PeerRecord, PeerStore};
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure};
use coqui_transport_pipe::{Listener as PipeListener, OsIdentity};

use super::{
    Context, DEFAULT_SESSION, Desktop, GuiState, KindDesktop, approve, bad, busy, denied, permits,
    refuse,
};
use crate::access::Sources;
use crate::config::{GuiConfig, Indicator};
use crate::limit::Slots;
use crate::link::{GuiChange, NodeEvent, NodeEvents};
use crate::node::{Node, NodeError};
use crate::policy::{Grant, Permission};

// ---------------------------------------------------------------------------------------------------------------- accounts

/// An account as the operating system names it: a SID on Windows, a decimal uid on Unix. `text` is that already, or a name.
///
/// # Errors
///
/// Why the name is not an account of this machine.
pub fn resolve_account(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("an account is not empty".to_owned());
    }
    resolve(text)
}

#[cfg(windows)]
fn resolve(text: &str) -> Result<String, String> {
    if text.len() > 4 && text[..4].eq_ignore_ascii_case("S-1-") {
        let sid = text.to_ascii_uppercase();
        return if sid[2..].chars().all(|c| c.is_ascii_digit() || c == '-') {
            Ok(sid)
        } else {
            Err(format!("`{text}` is not a SID"))
        };
    }
    // The three service accounts a node runs as, without asking the system.
    match text.to_ascii_lowercase().as_str() {
        "nt authority\\system" | "system" => return Ok("S-1-5-18".to_owned()),
        "nt authority\\localservice" | "localservice" => return Ok("S-1-5-19".to_owned()),
        "nt authority\\networkservice" | "networkservice" => return Ok("S-1-5-20".to_owned()),
        _ => {}
    }
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let shell = std::path::Path::new(&root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    // The name travels in the environment, never in the command text.
    let output = std::process::Command::new(shell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "try { ([System.Security.Principal.NTAccount]$env:BOHIO_ACCOUNT).Translate([System.Security.Principal.SecurityIdentifier]).Value } catch { exit 1 }",
        ])
        .env("BOHIO_ACCOUNT", text)
        .env_remove("PSModulePath")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("`{text}` cannot be looked up: {error}"))?;
    let answer = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && answer.starts_with("S-1-") {
        Ok(answer.to_ascii_uppercase())
    } else {
        Err(format!("`{text}` is not an account of this machine"))
    }
}

#[cfg(unix)]
fn resolve(text: &str) -> Result<String, String> {
    if text.chars().all(|c| c.is_ascii_digit()) {
        return text
            .parse::<u32>()
            .map(|uid| uid.to_string())
            .map_err(|_| format!("`{text}` is not a uid"));
    }
    let passwd = std::fs::read_to_string("/etc/passwd")
        .map_err(|error| format!("`{text}` cannot be looked up in /etc/passwd: {error}"))?;
    passwd
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let uid = fields.nth(1)?;
            (name == text).then(|| uid.to_owned())
        })
        .next()
        .ok_or_else(|| format!("`{text}` is not an account of this machine (use its uid for one /etc/passwd does not list)"))
}

/// Whether `user`, the account the operating system says is at the other end of a connection, is one of `accounts` (each as
/// [`resolve_account`] gives it). No answer from the system is a refusal.
pub fn admits(user: Option<&str>, accounts: &[String]) -> bool {
    user.is_some_and(|user| {
        accounts
            .iter()
            .any(|account| account.eq_ignore_ascii_case(user))
    })
}

/// Resolves a list of accounts, naming the first that is not one.
///
/// # Errors
///
/// The reason, with the account.
pub fn resolve_accounts(accounts: &[String]) -> Result<Vec<String>, String> {
    accounts.iter().map(|text| resolve_account(text)).collect()
}

// ------------------------------------------------------------------------------------------------------- the node's registry

/// One connected helper.
pub(super) struct Entry {
    /// How a request names the session: the verified account, then the helper's own name.
    pub(super) id: String,
    user: Option<String>,
    /// The exec source that runs the helper, when one does: a peer reaches the session only with that source.
    source: Option<String>,
    /// The peers with a relayed stream on this helper now, and how many each: the only ones whose audit events it may report.
    relaying: Mutex<std::collections::HashMap<String, (usize, usize)>>,
    opener: Opener,
    state: Mutex<State>,
}

impl Entry {
    /// Whether `grant` may use the machine behind this session: the source that runs the helper, when there is one.
    pub(super) fn reachable(&self, sources: &Sources, grant: &Grant) -> bool {
        self.source
            .as_deref()
            .is_none_or(|name| sources.permits(grant, name))
    }

    fn session(&self) -> GuiSession {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        GuiSession {
            id: self.id.clone(),
            user: self.user.clone(),
            active: state.active,
            locked: state.locked,
            displays: state.displays.clone(),
        }
    }

    /// Notes a relayed stream of `peer`; the helper may report events for it while the stream runs, and until its `ended`.
    pub(super) fn relay(self: &Arc<Self>, peer: String) -> Relaying {
        let mut relaying = self.relaying.lock().unwrap_or_else(PoisonError::into_inner);
        let counts = relaying.entry(peer.clone()).or_default();
        counts.0 += 1;
        counts.1 += 1;
        drop(relaying);
        Relaying {
            entry: Arc::clone(self),
            peer,
        }
    }

    /// Whether the helper may report an event for `peer` now. `last` is an event that ends a stream, which is the last
    /// one it owes for it.
    fn accepts_event(&self, peer: &str, last: bool) -> bool {
        let mut relaying = self.relaying.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(counts) = relaying.get_mut(peer) else {
            return false;
        };
        if last {
            counts.1 = counts.1.saturating_sub(1);
            if *counts == (0, 0) {
                relaying.remove(peer);
            }
        }
        true
    }

    fn active(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active
    }
}

/// A relayed stream of one peer on a helper, noted while it runs.
pub(super) struct Relaying {
    entry: Arc<Entry>,
    peer: String,
}

impl Drop for Relaying {
    fn drop(&mut self) {
        let mut relaying = self
            .entry
            .relaying
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(counts) = relaying.get_mut(&self.peer) {
            counts.0 -= 1;
            if *counts == (0, 0) {
                relaying.remove(&self.peer);
            }
        }
    }
}

/// The helpers a node holds, and how many it takes.
pub(super) struct Helpers {
    entries: Mutex<Vec<Arc<Entry>>>,
    slots: Slots,
}

impl Helpers {
    pub(super) fn new(max: usize) -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            slots: Slots::new(max),
        }
    }

    /// Adds `entry`; a helper of the same account and session that is still listed is replaced (it is stale: only one process
    /// of an account serves a session) and its streams end.
    fn register(&self, entry: Arc<Entry>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|known| {
            if known.id == entry.id {
                known.opener.close();
                false
            } else {
                true
            }
        });
        entries.push(entry);
    }

    fn remove(&self, entry: &Arc<Entry>) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|known| !Arc::ptr_eq(known, entry));
    }

    /// The sessions of the helpers `visible` admits.
    pub(super) fn sessions(&self, visible: impl Fn(&Entry) -> bool) -> Vec<GuiSession> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|entry| entry.opener.is_up() && visible(entry))
            .map(|entry| entry.session())
            .collect()
    }

    fn find(&self, id: &str, visible: impl Fn(&Entry) -> bool) -> Option<Arc<Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|entry| entry.id == id && entry.opener.is_up() && visible(entry))
            .cloned()
    }

    /// The helper a request without a session names: the one at the screen, else the only one.
    fn default_for(
        &self,
        visible: impl Fn(&Entry) -> bool,
    ) -> Result<Option<Arc<Entry>>, ErrorBody> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let up: Vec<&Arc<Entry>> = entries
            .iter()
            .filter(|entry| entry.opener.is_up() && visible(entry))
            .collect();
        match up.as_slice() {
            [] => Ok(None),
            [only] => Ok(Some(Arc::clone(only))),
            many => {
                let active: Vec<&&Arc<Entry>> =
                    many.iter().filter(|entry| entry.active()).collect();
                if let [one] = active.as_slice() {
                    return Ok(Some(Arc::clone(one)));
                }
                let names: Vec<&str> = many.iter().map(|entry| entry.id.as_str()).collect();
                Err(bad(&format!(
                    "name one of the sessions: {}",
                    names.join(", ")
                )))
            }
        }
    }
}

/// Where a `bohio-gui/1` open goes.
pub(super) enum Chosen {
    /// The node's own desktop answers it (or the engine says why not).
    InProcess,
    /// A helper's session.
    Helper(Arc<Entry>),
    /// The open is refused with this.
    Refuse(ErrorBody),
}

impl GuiState {
    /// Decides where a `bohio-gui/1` open goes: a session a request names that a helper serves goes to it; otherwise the node's
    /// own desktop when it answers; otherwise the helper at the screen, or the only one.
    pub(super) fn choose(&self, config: &GuiConfig, grant: &Grant, metadata: &[u8]) -> Chosen {
        let Ok(request) = GuiRequest::from_metadata(metadata) else {
            return Chosen::InProcess;
        };
        if !config.enabled || request.source.is_some() {
            return Chosen::InProcess;
        }
        let visible = |entry: &Entry| entry.reachable(&self.sources, grant);
        match request.session.as_deref() {
            Some(id) if id != DEFAULT_SESSION => self
                .helpers
                .find(id, visible)
                .map_or(Chosen::InProcess, Chosen::Helper),
            _ => {
                if self.in_process_available() {
                    return Chosen::InProcess;
                }
                match self.helpers.default_for(visible) {
                    Ok(Some(entry)) => Chosen::Helper(entry),
                    Ok(None) => Chosen::InProcess,
                    Err(error) => Chosen::Refuse(error),
                }
            }
        }
    }
}

/// The bounds the node hands a helper.
fn policy_of(config: &GuiConfig) -> HelperPolicy {
    HelperPolicy {
        indicator_notify: config.indicator == Indicator::Notify,
        input_when_locked: config.input_when_locked,
        input_when_lock_unknown: config.input_when_lock_unknown,
        max_input_rate: config.max_input_rate,
        max_snapshot_pixels: config.max_snapshot_pixels,
        max_batch_events: config.max_batch_events as u64,
        max_text: config.max_text as u64,
        clipboard_max_bytes: config.clipboard_max_bytes as u64,
        max_fps: config.max_fps,
    }
}

/// `user`'s `[gui]` narrowed by the node's `policy`: for each bound the stricter of the two, and a notice when either asks for
/// one. The helper never serves more than the node allows and never less than its user asked for.
#[must_use]
pub fn narrowed(user: &GuiConfig, policy: &HelperPolicy) -> GuiConfig {
    let mut config = user.clone();
    // Starting the helper is the user's consent to lend the desktop: the node's `enabled` and the user's own default do not
    // switch it off again.
    config.enabled = true;
    if policy.indicator_notify {
        config.indicator = Indicator::Notify;
    }
    config.input_when_locked = config.input_when_locked && policy.input_when_locked;
    config.input_when_lock_unknown =
        config.input_when_lock_unknown && policy.input_when_lock_unknown;
    config.max_input_rate = config.max_input_rate.min(policy.max_input_rate);
    config.max_snapshot_pixels = config.max_snapshot_pixels.min(policy.max_snapshot_pixels);
    config.max_batch_events = config
        .max_batch_events
        .min(usize::try_from(policy.max_batch_events).unwrap_or(usize::MAX));
    config.max_text = config
        .max_text
        .min(usize::try_from(policy.max_text).unwrap_or(usize::MAX));
    config.clipboard_max_bytes = config
        .clipboard_max_bytes
        .min(usize::try_from(policy.clipboard_max_bytes).unwrap_or(usize::MAX));
    config.max_fps = config.max_fps.min(policy.max_fps);
    config.default_fps = config.default_fps.min(config.max_fps);
    config.min_fps = config.min_fps.min(config.max_fps);
    config
}

// ------------------------------------------------------------------------------------------- the node: relaying a stream

/// What the helper's permissions are for a stream the node decided: exactly what was granted.
fn grant_of(granted: &[Want]) -> Grant {
    Grant::allowing(granted.iter().map(|want| match want {
        Want::View => Permission::GuiView,
        Want::Input => Permission::GuiInput,
        Want::Clipboard => Permission::GuiClipboard,
    }))
}

/// A client frame the peer's grant no longer allows, answered at once and not passed on: the permission the frame needs and
/// the helper's source, read now, as the engine does for each request.
fn refused_now(frame: &[u8], context: &Context<'_>, entry: &Entry) -> Option<RequestError> {
    let (seq, want) = match GuiInput::decode(frame).ok()? {
        GuiInput::Snapshot(request) => (request.seq, Want::View),
        GuiInput::Settle(request) => (request.seq, Want::View),
        GuiInput::Watch(request) => (request.seq, Want::View),
        GuiInput::Input(request) => (request.seq, Want::Input),
        GuiInput::ClipboardGet(request) => (request.seq, Want::Clipboard),
        GuiInput::ClipboardSet(request) => (request.seq, Want::Clipboard),
        _ => return None,
    };
    let now = match context.current {
        Some(current) => match (current.0)() {
            Ok(grant) => grant,
            Err(error) => return Some(RequestError::new(seq, error)),
        },
        None => context.grant.clone(),
    };
    if !permits(&now, want) {
        return Some(RequestError::new(
            seq,
            denied(&format!("this peer may no longer {want} the desktop")),
        ));
    }
    if !entry.reachable(&context.state.sources, &now) {
        return Some(RequestError::new(
            seq,
            denied("this peer may no longer use that desktop source"),
        ));
    }
    if matches!(want, Want::View)
        && !matches!(
            GuiInput::decode(frame),
            Ok(GuiInput::Watch(_)) // a watch is bounded by its frames, not by the snapshot rate
        )
        && !context.state.snapshots.admit(&context.peer.to_string())
    {
        return Some(RequestError::new(
            seq,
            ErrorBody::new(
                ErrorCode::Busy,
                "this peer has taken as many snapshots a minute as the host allows",
            ),
        ));
    }
    None
}

fn session_ended() -> ErrorBody {
    ErrorBody::new(ErrorCode::Unavailable, "the session ended")
}

/// Serves one peer's `bohio-gui/1` stream through the helper `entry`: the node's own checks (the grant, the limits, the person's
/// approval) and then the bytes both ways. The helper's refusals and its frames reach the peer unchanged.
pub(super) fn serve_relayed(
    channel: &Arc<dyn Channel>,
    entry: &Arc<Entry>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<(), ErrorBody> {
    let result = relayed(channel, entry, metadata, context);
    if let Err(error) = result {
        return Err(refuse(channel.as_ref(), error, context));
    }
    Ok(())
}

fn relayed(
    channel: &Arc<dyn Channel>,
    entry: &Arc<Entry>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<(), ErrorBody> {
    let config = context.config;
    let request =
        GuiRequest::from_metadata(metadata).map_err(|_| bad("the request cannot be read"))?;
    if request.want.as_ref().is_some_and(Vec::is_empty) {
        return Err(bad("name at least one thing to want"));
    }
    let wants = request.wants();
    if let Some(missing) = wants.iter().find(|want| !permits(context.grant, **want)) {
        return Err(denied(&format!("this peer may not {missing} the desktop")));
    }
    if !entry.reachable(&context.state.sources, context.grant) {
        return Err(denied("this peer may not use that desktop source"));
    }
    let _slot = context.state.slots.acquire().ok_or_else(busy)?;
    let _mine = context
        .state
        .per_peer
        .take(context.peer.to_string())
        .ok_or_else(busy)?;
    approve(channel.as_ref(), context, &wants)?;
    let open = HelperOpen {
        request: GuiRequest {
            session: Some(entry.id.clone()),
            want: Some(wants.clone()),
            source: None,
        },
        peer: context.peer.to_string(),
        session: entry.id.clone(),
        granted: wants,
        policy: policy_of(config),
    }
    .to_metadata()
    .map_err(|_| ErrorBody::new(ErrorCode::Internal, "the open cannot be written"))?;
    let remote = entry
        .opener
        .open_stream(protocol::GUI, &open)
        .map_err(|_| session_ended())?;
    let _relaying = entry.relay(context.peer.to_string());
    let poll = config.poll_interval.0.max(Duration::from_millis(1));
    let done = AtomicBool::new(false);
    let gone = Gone::default();
    thread::scope(|scope| {
        let (done_ref, remote_ref, gone_ref) = (&done, &remote, &gone);
        scope.spawn(move || {
            downstream(remote_ref, channel.as_ref(), done_ref, poll, gone_ref);
            done_ref.store(true, Ordering::SeqCst);
        });
        upstream(
            channel.as_ref(),
            &remote,
            context,
            &done,
            poll,
            entry,
            &gone,
        );
        done.store(true, Ordering::SeqCst);
    });
    // A helper that went away says nothing of itself: the peer is told why its stream ends, unless the helper said why.
    let helper_ended = gone.ended.load(Ordering::SeqCst) && !gone.said_why.load(Ordering::SeqCst);
    if (helper_ended || !entry.opener.is_up()) && !channel.is_closed() {
        let frame = GuiOutput::Error(session_ended())
            .encode()
            .unwrap_or_else(|_| vec![0x05]);
        let _ = channel.send(&frame);
    }
    remote.close();
    Ok(())
}

/// Peer to helper: each frame is checked against the grant as it is now, then passed on.
fn upstream(
    from: &dyn Channel,
    to: &Opened,
    context: &Context<'_>,
    done: &AtomicBool,
    poll: Duration,
    entry: &Entry,
    gone: &Gone,
) {
    while !done.load(Ordering::SeqCst) {
        if !entry.opener.is_up() {
            return gone.ended.store(true, Ordering::SeqCst);
        }
        match from.receive(Some(poll)) {
            Ok(Some(frame)) => {
                if let Some(refusal) = refused_now(&frame, context, entry) {
                    let reply = GuiOutput::RequestError(refusal)
                        .encode()
                        .unwrap_or_else(|_| vec![0x05]);
                    if from.send(&reply).is_err() {
                        return done.store(true, Ordering::SeqCst);
                    }
                    continue;
                }
                if to.send(&frame).is_err() {
                    gone.ended.store(true, Ordering::SeqCst);
                    return done.store(true, Ordering::SeqCst);
                }
            }
            Err(Failure::Timeout) => {}
            Ok(None) | Err(_) => return done.store(true, Ordering::SeqCst),
        }
    }
}

/// How a relayed stream ended on the helper's side.
#[derive(Default)]
struct Gone {
    /// The helper's stream ended on its own, before this side finished it.
    ended: AtomicBool,
    /// The helper sent its own last frame (an `error`) first.
    said_why: AtomicBool,
}

/// Helper to peer: every frame as it came.
fn downstream(from: &Opened, to: &dyn Channel, done: &AtomicBool, poll: Duration, gone: &Gone) {
    while !done.load(Ordering::SeqCst) {
        match from.receive(Some(poll)) {
            Ok(Some(frame)) => {
                if frame.first() == Some(&0x05) {
                    gone.said_why.store(true, Ordering::SeqCst);
                }
                if to.send(&frame).is_err() {
                    return done.store(true, Ordering::SeqCst);
                }
            }
            Err(Failure::Timeout) => {}
            Ok(None) | Err(_) => {
                gone.ended.store(true, Ordering::SeqCst);
                return done.store(true, Ordering::SeqCst);
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------- the node: the endpoint

/// The bound helper endpoint of a node, and the accounts it serves.
#[derive(Debug)]
pub struct HelperListener {
    listener: PipeListener,
    endpoint: Endpoint,
    accounts: Vec<String>,
    refused_log: Duration,
}

impl HelperListener {
    /// Binds the endpoint `config` names for the node `node`, when `[gui] helper_accounts` is not empty.
    ///
    /// # Errors
    ///
    /// [`NodeError::Listen`] for an account that is not one, an endpoint that is not a `pipe://` one or cannot be bound, and on
    /// Unix a socket directory another account may write.
    pub fn bind(config: &GuiConfig, node: &PeerId) -> Result<Option<Self>, NodeError> {
        if config.helper_accounts.is_empty() {
            return Ok(None);
        }
        let text = config.helper_endpoint.clone().unwrap_or_else(|| {
            let id = node.to_string();
            let digits = id.rsplit(':').next().unwrap_or("");
            format!("pipe://bohio-gui-{}", &digits[..digits.len().min(16)])
        });
        let fail = |reason: String| NodeError::Listen {
            endpoint: text.clone(),
            reason,
        };
        let accounts = resolve_accounts(&config.helper_accounts)
            .map_err(|reason| fail(format!("gui.helper_accounts: {reason}")))?;
        let Endpoint::Pipe(name) = Endpoint::parse(&text).map_err(fail)? else {
            return Err(fail("the helper endpoint is a `pipe://` one".to_owned()));
        };
        let mut builder = PipeListener::builder().name(&name);
        #[cfg(unix)]
        {
            only_this_account_writes(name.path()).map_err(fail)?;
            builder = builder.socket_mode(0o666);
        }
        #[cfg(windows)]
        {
            // Interactive users may connect; the system and the node's own account have it all.
            let me = coqui_transport_pipe::current_user()
                .map_err(|error| fail(format!("the node's account is not known: {error}")))?;
            builder = builder
                .security_descriptor(Some(format!("D:P(A;;GA;;;SY)(A;;GA;;;{me})(A;;GRGW;;;IU)")));
        }
        let listener = builder.build().map_err(|error| fail(error.to_string()))?;
        Ok(Some(Self {
            listener,
            endpoint: Endpoint::Pipe(name),
            accounts,
            refused_log: config.refused_log_interval.0,
        }))
    }

    /// The endpoint as bound.
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// A listener over `accounts` (resolved), for a test of the check alone.
    #[doc(hidden)]
    pub fn accounts(&self) -> &[String] {
        &self.accounts
    }
}

/// The directory of the socket must be one only the node's account may write, or another account could replace the socket by a
/// name of its own.
#[cfg(unix)]
fn only_this_account_writes(socket: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;
    let directory = socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "the helper socket has no directory".to_owned())?;
    let metadata = std::fs::metadata(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    let me = coqui_transport_pipe::current_user().map_err(|error| error.to_string())?;
    if metadata.uid().to_string() != me {
        return Err(format!(
            "{} belongs to another account: the helper socket goes in a directory only the node's account owns",
            directory.display()
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(format!(
            "{} may be written by another account: make it mode 0755 or tighter",
            directory.display()
        ));
    }
    Ok(())
}

/// The store of a node's helper connection: the one protocol a helper opens.
struct HelperStore;

impl PeerStore for HelperStore {
    fn get(&self, peer: &str) -> Result<Option<PeerRecord>, cabuya::Error> {
        Ok(Some(PeerRecord::new(peer).granting(GUI_HELPER)))
    }

    fn put(&self, _record: &PeerRecord) -> Result<(), cabuya::Error> {
        Ok(())
    }

    fn remove(&self, _peer: &str) -> Result<(), cabuya::Error> {
        Ok(())
    }
}

/// What one helper connection shares between its handler and the loop that serves it.
#[derive(Default)]
struct Connection {
    /// The helper said who it serves.
    registered: AtomicBool,
    /// The helper is gone, or was replaced: the connection ends.
    ended: AtomicBool,
}

impl Node {
    /// Serves helpers of the accounts `helpers` admits until `stop` is set, each on a thread of its own. A connection whose
    /// account the operating system does not say is listed is closed unread.
    ///
    /// # Errors
    ///
    /// The listener's error.
    pub fn serve_helpers(&self, helpers: &HelperListener, stop: &AtomicBool) -> io::Result<()> {
        let (mut logged, mut unlisted) = (None, 0_u64);
        let parts = self.gui_parts();
        thread::scope(|scope| {
            while !stop.load(Ordering::SeqCst) {
                let connection = match helpers
                    .listener
                    .accept_timeout(Some(parts.settings.accept_wait))
                {
                    Ok(connection) => connection,
                    Err(coqui_transport_pipe::Error::TimedOut) => continue,
                    Err(error) => return Err(error.into()),
                };
                let identity = connection.peer().ok();
                let Some(account) = admitted(identity.as_ref(), &helpers.accounts) else {
                    unlisted += 1;
                    if logged.is_none_or(|at: Instant| at.elapsed() >= helpers.refused_log) {
                        logged = Some(Instant::now());
                        tracing::info!(
                            count = unlisted,
                            "connections to the helper endpoint from an account that is not listed were closed unread"
                        );
                        parts.events.event(&NodeEvent::Gui {
                            peer: parts.host,
                            session: "helper".to_owned(),
                            change: GuiChange::Refused {
                                reason: format!(
                                    "{unlisted} helper connection(s) from an account that is not listed were closed unread"
                                ),
                            },
                        });
                        unlisted = 0;
                    }
                    continue;
                };
                let Some(permit) = parts.state.helpers.slots.acquire() else {
                    tracing::info!(
                        "a helper was closed unread: the node holds as many as it allows"
                    );
                    continue;
                };
                let Ok(channel) = bohio_client::link::stream(connection, &parts.settings) else {
                    continue;
                };
                scope.spawn(move || {
                    let _permit = permit;
                    self.serve_helper_channel(&channel, &account, Some(stop));
                    channel.close();
                });
            }
            Ok(())
        })
    }

    /// Serves one helper of the account `account` (as [`resolve_account`] writes it) on `channel`, an unproven framed channel,
    /// until it ends or `stop` is set. The account is the caller's check: nothing the helper sends changes it.
    pub fn serve_helper_channel<C: Channel>(
        &self,
        channel: &C,
        account: &str,
        stop: Option<&AtomicBool>,
    ) {
        let parts = self.gui_parts();
        let settings = parts.settings;
        channel.set_max_message(settings.max_frame.saturating_add(5));
        let _ = channel.set_pending(settings.read_ahead);
        let (opener, opens) = Opener::pair(&settings);
        let shared = Arc::new(Connection::default());
        let services = Services::default().with(
            GUI_HELPER,
            Box::new({
                let (state, events, opener) = (
                    Arc::clone(&parts.state),
                    Arc::clone(&parts.events),
                    opener.clone(),
                );
                let (shared, account) = (Arc::clone(&shared), account.to_owned());
                move |stream: &dyn LinkStream, accepted: &Accepted| {
                    hello(
                        &Held {
                            state: &state,
                            events: events.as_ref(),
                            opener: &opener,
                            account: &account,
                            shared: &shared,
                            poll: settings.poll_interval.max(Duration::from_millis(1)),
                        },
                        stream,
                        accepted,
                    );
                }
            }),
        );
        let host = Host {
            store: &HelperStore,
            services: &services,
            events: &NoEvents,
        };
        let mut options = Options::default();
        options.limits = settings.limits();
        let finished = AtomicBool::new(false);
        let started = Instant::now();
        thread::scope(|scope| {
            scope.spawn(|| {
                while !finished.load(Ordering::SeqCst) {
                    let silent = !shared.registered.load(Ordering::SeqCst)
                        && started.elapsed() > settings.proof_timeout;
                    if stop.is_some_and(|stop| stop.load(Ordering::SeqCst))
                        || shared.ended.load(Ordering::SeqCst)
                        || silent
                    {
                        channel.close();
                        return;
                    }
                    thread::sleep(settings.poll_interval.max(Duration::from_millis(1)));
                }
            });
            serve_with(
                channel,
                Side::Answered,
                options,
                None,
                "helper",
                &host,
                |mux| opens.run(mux, &settings),
            );
            finished.store(true, Ordering::SeqCst);
        });
        opener.close();
    }
}

fn admitted(identity: Option<&OsIdentity>, accounts: &[String]) -> Option<String> {
    let user = identity.and_then(|who| who.user.as_deref())?;
    admits(Some(user), accounts).then(|| user.to_ascii_uppercase())
}

/// What the handler of a helper's first stream holds.
struct Held<'a> {
    state: &'a GuiState,
    events: &'a dyn NodeEvents,
    opener: &'a Opener,
    account: &'a str,
    shared: &'a Connection,
    poll: Duration,
}

/// `bohio-gui-helper/1`: lists the helper's session under its verified account, keeps its state, passes its audit events on, and
/// ends with it.
fn hello(held: &Held<'_>, stream: &dyn LinkStream, accepted: &Accepted) {
    let Ok(hello) = HelperHello::from_metadata(&accepted.metadata) else {
        return stream.close_with(CloseReason::Refused, "that is not a helper hello");
    };
    if hello.session.is_empty() || hello.session.len() > 128 {
        return stream.close_with(
            CloseReason::Refused,
            "the session name is empty or too long",
        );
    }
    if held.shared.registered.swap(true, Ordering::SeqCst) {
        return stream.close_with(CloseReason::Refused, "a connection serves one session");
    }
    let entry = Arc::new(Entry {
        id: format!("{}:{}", held.account, hello.session),
        user: hello.user,
        source: held.account.strip_prefix("exec:").map(str::to_owned),
        relaying: Mutex::default(),
        opener: held.opener.clone(),
        state: Mutex::new(hello.state),
    });
    held.state.helpers.register(Arc::clone(&entry));
    while held.opener.is_up() {
        match stream.receive(Some(held.poll)) {
            Ok(Some(message)) => match HelperMessage::decode(&message) {
                Ok(HelperMessage::State { state }) => {
                    *entry.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
                }
                Ok(HelperMessage::Event { peer, change }) => {
                    // A helper reports what it served for a peer that has a stream on it now, and no other peer's.
                    let last = matches!(
                        change,
                        HelperChange::Ended { .. } | HelperChange::Refused { .. }
                    );
                    if !entry.accepts_event(&peer, last) {
                        tracing::debug!("a helper reported an event for a peer it is not serving");
                    } else if let Ok(peer) = peer.parse::<PeerId>() {
                        held.events.event(&NodeEvent::Gui {
                            peer,
                            session: entry.id.clone(),
                            change: change_of(change),
                        });
                    }
                }
                Err(_) => break,
            },
            Err(Failure::Timeout) => {
                if stream.life() != StreamLife::Open {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    held.state.helpers.remove(&entry);
    held.opener.close();
    held.shared.ended.store(true, Ordering::SeqCst);
}

fn change_of(change: HelperChange) -> GuiChange {
    match change {
        HelperChange::Started { granted } => GuiChange::Started { granted },
        HelperChange::Ended {
            snapshots,
            frames,
            input_events,
            clipboard_reads,
            clipboard_writes,
            reason,
        } => GuiChange::Ended {
            snapshots,
            frames,
            input_events,
            clipboard_reads,
            clipboard_writes,
            reason,
        },
        HelperChange::ClipboardRead => GuiChange::ClipboardRead,
        HelperChange::ClipboardWritten => GuiChange::ClipboardWritten,
        HelperChange::Refused { reason } => GuiChange::Refused { reason },
    }
}

fn helper_change(change: &GuiChange) -> HelperChange {
    match change {
        GuiChange::Started { granted } => HelperChange::Started {
            granted: granted.clone(),
        },
        GuiChange::Ended {
            snapshots,
            frames,
            input_events,
            clipboard_reads,
            clipboard_writes,
            reason,
        } => HelperChange::Ended {
            snapshots: *snapshots,
            frames: *frames,
            input_events: *input_events,
            clipboard_reads: *clipboard_reads,
            clipboard_writes: *clipboard_writes,
            reason: reason.clone(),
        },
        GuiChange::ClipboardRead => HelperChange::ClipboardRead,
        GuiChange::ClipboardWritten => HelperChange::ClipboardWritten,
        GuiChange::Refused { reason } => HelperChange::Refused {
            reason: reason.clone(),
        },
    }
}

// ------------------------------------------------------------------------------------------------------------- the helper

/// What a helper runs with.
#[derive(Debug, Clone)]
pub struct HelperSettings {
    /// The node's helper endpoint.
    pub endpoint: Endpoint,
    /// The account that must serve the endpoint, a SID or name (Windows) or a uid or name (Unix).
    pub node_account: String,
    /// The helper's own name for its session, which the node lists after the account.
    pub session: String,
    /// The account's name for a person.
    pub user: Option<String>,
    /// The user's own `[gui]`: the node's bounds only narrow it.
    pub gui: GuiConfig,
    /// The link's settings (frame, window, poll).
    pub link: Settings,
}

/// Why a helper did not run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HelperError {
    /// The node's account is not one this machine knows.
    #[error("{0}")]
    Account(String),
    /// Nothing answers on the endpoint.
    #[error("the node's helper endpoint cannot be reached: {0}")]
    Unreachable(String),
    /// Another account serves the endpoint: nothing was sent to it.
    #[error(
        "the helper endpoint is served by {found}, not by the node's account {expected}: nothing was sent"
    )]
    NotTheNode {
        /// Who serves it, as the system says.
        found: String,
        /// Who the helper was told to trust.
        expected: String,
    },
    /// The endpoint is not a pipe or the link failed.
    #[error("{0}")]
    Link(String),
}

/// The audit events of the streams a helper serves, queued for its first stream.
struct HelperEvents(Mutex<Sender<HelperMessage>>);

impl NodeEvents for HelperEvents {
    fn event(&self, event: &NodeEvent) {
        if let NodeEvent::Gui { peer, change, .. } = event {
            let message = HelperMessage::Event {
                peer: peer.to_string(),
                change: helper_change(change),
            };
            let _ = self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(message);
        }
    }
}

/// The grants of a helper's connection to its node: the one protocol the node opens.
struct NodeStore;

impl PeerStore for NodeStore {
    fn get(&self, peer: &str) -> Result<Option<PeerRecord>, cabuya::Error> {
        Ok(Some(PeerRecord::new(peer).granting(protocol::GUI)))
    }

    fn put(&self, _record: &PeerRecord) -> Result<(), cabuya::Error> {
        Ok(())
    }

    fn remove(&self, _peer: &str) -> Result<(), cabuya::Error> {
        Ok(())
    }
}

/// The state of `sources`' desktop as the helper reports it; not at the screen when it cannot be asked.
fn state_of(state: &GuiState) -> (State, Option<String>) {
    let Ok(source) = state.sources.desktop(None) else {
        return (State::default(), None);
    };
    let Ok(desktop) = KindDesktop::open(source) else {
        return (State::default(), None);
    };
    match state.call(|| desktop.state()) {
        Ok(found) => (found.wire(), found.user),
        Err(_) => (State::default(), None),
    }
}

/// Runs a helper: connects to the node's endpoint, checks that the node's account serves it, says which session it serves, and
/// answers the node's streams until the node closes the connection or `stop` is set.
///
/// The check comes first: before the helper sends anything it asks the operating system who serves the endpoint (on Windows the
/// owner of the pipe, on Unix the server's uid) and refuses any account but `node_account`, because what a helper serves is its
/// user's whole desktop.
///
/// # Errors
///
/// [`HelperError`]; `Ok` when the node closed the connection.
pub fn run(
    settings: &HelperSettings,
    sources: Arc<Sources>,
    stop: &AtomicBool,
) -> Result<(), HelperError> {
    use coqui_transport::Connector as _;
    let expected = resolve_account(&settings.node_account)
        .map_err(|reason| HelperError::Account(format!("the node's account: {reason}")))?;
    let Endpoint::Pipe(name) = &settings.endpoint else {
        return Err(HelperError::Link(
            "the helper endpoint is a `pipe://` one".to_owned(),
        ));
    };
    let pipe = coqui_transport_pipe::Connector::new(name.clone())
        .connect(settings.link.proof_timeout)
        .map_err(|error| HelperError::Unreachable(error.to_string()))?;
    let found = pipe.peer().ok().and_then(|who| who.user);
    if !admits(found.as_deref(), std::slice::from_ref(&expected)) {
        return Err(HelperError::NotTheNode {
            found: found.unwrap_or_else(|| "an account the system did not name".to_owned()),
            expected: format!("{} ({expected})", settings.node_account),
        });
    }
    let channel = bohio_client::link::stream(pipe, &settings.link)
        .map_err(|error| HelperError::Link(error.to_string()))?;
    serve_node(&channel, settings, sources, stop);
    channel.close();
    Ok(())
}

/// Serves the node on `channel`, which the caller has already checked is the node's: the hello, the state, and the node's streams.
/// [`run`] is the caller that checks.
pub fn serve_node<C: Channel>(
    channel: &C,
    settings: &HelperSettings,
    sources: Arc<Sources>,
    stop: &AtomicBool,
) {
    let link = settings.link;
    let state = GuiState::new(&settings.gui, Arc::clone(&sources));
    #[cfg(feature = "notify")]
    state.set_notifier(Arc::new(super::DesktopNotifier::new()));
    let (sender, receiver) = mpsc::channel();
    let receiver = Mutex::new(receiver);
    let events = Arc::new(HelperEvents(Mutex::new(sender)));
    let services = Services::default().with(
        protocol::GUI,
        Box::new({
            let (state, events, user) = (
                Arc::clone(&state),
                Arc::clone(&events),
                settings.gui.clone(),
            );
            move |stream: &dyn LinkStream, accepted: &Accepted| {
                serve_stream(&state, &user, events.as_ref(), &link, stream, accepted);
            }
        }),
    );
    let host = Host {
        store: &NodeStore,
        services: &services,
        events: &NoEvents,
    };
    let mut options = Options::default();
    options.limits = link.limits();
    channel.set_max_message(link.max_frame.saturating_add(5));
    let _ = channel.set_pending(link.read_ahead);
    serve_with(channel, Side::Dialed, options, None, "node", &host, |mux| {
        announce(mux, channel, settings, &state, &receiver, stop)
    });
}

/// Opens the helper's first stream and keeps it: the hello, then each change of state and each audit event, until the node closes it.
fn announce<C: Channel>(
    mux: &Mux<'_, C>,
    channel: &C,
    settings: &HelperSettings,
    state: &Arc<GuiState>,
    events: &Mutex<Receiver<HelperMessage>>,
    stop: &AtomicBool,
) {
    let poll = settings.link.poll_interval.max(Duration::from_millis(1));
    let interval = settings.gui.state_interval.0;
    let (first, user) = state_of(state);
    let hello = HelperHello {
        session: settings.session.clone(),
        user: user.or_else(|| settings.user.clone()),
        state: first.clone(),
    };
    let Ok(metadata) = hello.to_metadata() else {
        return channel.close();
    };
    let Ok(stream) = mux.open_with(GUI_HELPER, &metadata) else {
        return channel.close();
    };
    let (mut last, mut next) = (first, Instant::now() + interval);
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match stream.receive(Some(Duration::ZERO)) {
            Err(Failure::Timeout) => {}
            // The node closed it, or said anything at all: this helper's work is over.
            _ => break,
        }
        if !matches!(mux.link_state(), cabuya::mux::LinkState::Up) {
            return;
        }
        let mut sent = true;
        while let Ok(message) = events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv_timeout(Duration::ZERO)
        {
            sent &= message
                .encode()
                .is_ok_and(|bytes| stream.send(&bytes).is_ok());
        }
        if Instant::now() >= next {
            next = Instant::now() + interval;
            let (now, _) = state_of(state);
            if now != last {
                last = now.clone();
                sent &= HelperMessage::State { state: now }
                    .encode()
                    .is_ok_and(|bytes| stream.send(&bytes).is_ok());
            }
        }
        if !sent {
            break;
        }
        thread::sleep(poll);
    }
    channel.close();
}

/// One of the node's streams: the engine over the helper's own desktop, for the peer and the grant the node named. The user's own
/// `[gui]` is narrowed by what the node sends and never widened.
fn serve_stream(
    state: &GuiState,
    user: &GuiConfig,
    events: &dyn NodeEvents,
    link: &Settings,
    stream: &dyn LinkStream,
    accepted: &Accepted,
) {
    let Ok(open) = HelperOpen::from_metadata(&accepted.metadata) else {
        return stream.close_with(CloseReason::Refused, "that is not a helper open");
    };
    let Ok(peer) = open.peer.parse::<PeerId>() else {
        return stream.close_with(CloseReason::Refused, "that is not a peer id");
    };
    let config = narrowed(user, &open.policy);
    let grant = grant_of(&open.granted);
    let request = GuiRequest {
        session: Some(open.session.clone()),
        want: Some(open.granted.clone()),
        source: None,
    };
    let Ok(request) = request.to_metadata() else {
        return stream.close_with(CloseReason::Refused, "the request cannot be written");
    };
    let context = Context {
        grant: &grant,
        current: None,
        config: &config,
        state,
        peer,
        events,
        session: &open.session,
    };
    thread::scope(|scope| {
        let relayed = relay(scope, stream, link);
        let channel: Arc<dyn Channel> = relayed.clone();
        let _ = super::serve(channel, &request, &context);
        relayed.close();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_answer_from_the_system_is_a_refusal() {
        let accounts = vec!["1000".to_owned()];
        assert!(!admits(None, &accounts));
        assert!(!admits(Some("1001"), &accounts));
        assert!(admits(Some("1000"), &accounts));
        assert!(!admits(Some("1000"), &[]));
    }

    #[test]
    fn a_sid_is_compared_without_regard_to_case() {
        let accounts = vec!["S-1-5-21-1-2-3-1001".to_owned()];
        assert!(admits(Some("s-1-5-21-1-2-3-1001"), &accounts));
        assert!(!admits(Some("S-1-5-21-1-2-3-100"), &accounts));
    }

    #[test]
    fn an_empty_account_is_not_one() {
        assert!(resolve_account("  ").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_numeric_uid_resolves_to_itself_and_root_by_name() {
        assert_eq!(resolve_account("1234").as_deref(), Ok("1234"));
        assert_eq!(resolve_account("root").as_deref(), Ok("0"));
        assert!(resolve_account("no-such-account-bohio").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn the_service_accounts_and_a_sid_resolve_without_the_system() {
        assert_eq!(
            resolve_account("NT AUTHORITY\\LocalService").as_deref(),
            Ok("S-1-5-19")
        );
        assert_eq!(resolve_account("s-1-5-18").as_deref(), Ok("S-1-5-18"));
        assert!(resolve_account("S-1-x").is_err());
    }

    #[test]
    fn the_nodes_bounds_narrow_the_users_and_never_widen_them() {
        let node = GuiConfig {
            max_input_rate: 10,
            input_when_locked: true,
            indicator: Indicator::Notify,
            ..GuiConfig::default()
        };
        let user = GuiConfig {
            max_input_rate: 50,
            indicator: Indicator::None,
            input_when_locked: false,
            max_text: 8,
            ..GuiConfig::default()
        };
        let merged = narrowed(&user, &policy_of(&node));
        assert_eq!(merged.max_input_rate, 10, "the node's lower rate wins");
        assert_eq!(merged.max_text, 8, "the user's lower bound wins");
        assert!(
            !merged.input_when_locked,
            "the user refuses what the node allows"
        );
        assert_eq!(
            merged.indicator,
            Indicator::Notify,
            "either side may ask for a notice"
        );
        assert!(merged.enabled);
        let node = GuiConfig {
            input_when_locked: false,
            ..GuiConfig::default()
        };
        let user = GuiConfig {
            input_when_locked: true,
            ..GuiConfig::default()
        };
        assert!(!narrowed(&user, &policy_of(&node)).input_when_locked);
    }

    #[test]
    fn a_named_session_is_the_account_then_the_helpers_own_name() {
        let helpers = Helpers::new(2);
        let (opener, _opens) = Opener::pair(&Settings::default());
        helpers.register(Arc::new(Entry {
            id: "1000:main".to_owned(),
            user: None,
            source: None,
            relaying: Mutex::default(),
            opener,
            state: Mutex::new(State::default()),
        }));
        assert!(helpers.find("1000:main", |_| true).is_some());
        assert!(
            helpers.find("main", |_| true).is_none(),
            "the helper's own name alone names nothing"
        );
    }
}

#[cfg(test)]
mod relaying_tests {
    use super::*;

    fn entry() -> Arc<Entry> {
        let (opener, _opens) = Opener::pair(&Settings::default());
        Arc::new(Entry {
            id: "1000:main".to_owned(),
            user: None,
            source: None,
            relaying: Mutex::default(),
            opener,
            state: Mutex::new(State::default()),
        })
    }

    #[test]
    fn a_helper_may_report_only_for_a_peer_it_is_serving_and_the_last_event_of_its_stream() {
        let entry = entry();
        assert!(
            !entry.accepts_event("stranger", false),
            "no stream, no word"
        );
        let stream = entry.relay("peer".to_owned());
        assert!(entry.accepts_event("peer", false));
        assert!(!entry.accepts_event("stranger", true));
        drop(stream);
        // The stream ended on the node's side; the helper's `ended` can still arrive, once.
        assert!(entry.accepts_event("peer", false));
        assert!(entry.accepts_event("peer", true));
        assert!(
            !entry.accepts_event("peer", false),
            "nothing is owed any more"
        );
    }
}
