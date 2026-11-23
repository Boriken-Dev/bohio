//! Detached sessions: an exec or a shell that keeps running when its stream ends, with its output kept for
//! the next attach (`bohio-attach/1`).
//!
//! A session is the ordinary engine ([`crate::exec`] or [`crate::shell`]) served on an in-process channel
//! that nothing closes when a client's stream does. [`Sessions`] sits on the other end of that channel: it
//! keeps what the program writes in a buffer with one byte offset across both output streams, and the
//! stream of whoever is attached (at most one) is fed from the buffer and its frames are passed to the
//! engine. A program whose output nobody has taken waits (`pause`) or has its oldest output forgotten
//! (`drop_oldest`), as the request chose.
//!
//! A session ends at its program's exit (kept for reading until `keep_exited`), at `end_session`, at its
//! `linger` while nobody is attached, when its owner's key or grant stops covering it, and with the host.

#![forbid(unsafe_code)]

mod pipe;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use bohio_proto::exec::{self, Exit};
use bohio_proto::fs::Timestamp;
use bohio_proto::session::{Attach, Attached, Kind, Listed, OnFull, SessionRequest};
use bohio_proto::shell;
use bohio_proto::signal::Signal;
use bohio_proto::{ErrorBody, ErrorCode};
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure};

use crate::config::{CompressionConfig, ExecutionConfig, SessionsConfig, ShellConfig};
use crate::limit::Slots;
use crate::link::{Grants, Keys, NodeEvent, NodeEvents, SessionChange};
use crate::policy::{Grant, Permission};

use self::pipe::{End, pair};

/// What an exec session needs from the host besides the request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExecHost<'a> {
    pub grant: &'a Grant,
    pub config: &'a ExecutionConfig,
    pub compression: &'a CompressionConfig,
    pub slots: &'a Arc<Slots>,
    pub path: &'a std::ffi::OsStr,
}

/// What a shell session needs from the host besides the request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShellHost<'a> {
    pub grant: &'a Grant,
    pub config: &'a ShellConfig,
    pub slots: &'a Arc<Slots>,
    pub path: &'a std::ffi::OsStr,
    pub sources: &'a Arc<crate::access::Sources>,
}

/// How an attached stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    /// The program's exit was delivered, or the session was ended.
    Done,
    /// Another stream attached.
    Superseded,
    /// The stream went away: the session is detached.
    Gone,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn busy(message: &'static str) -> ErrorBody {
    ErrorBody::new(ErrorCode::Busy, message)
}

fn not_found() -> ErrorBody {
    ErrorBody::new(ErrorCode::NotFound, "there is no such session")
}

/// A piece of the program's output.
struct Chunk {
    offset: u64,
    /// Standard error of an exec; everything else is standard output (a shell's terminal is `false`).
    stderr: bool,
    data: Vec<u8>,
}

struct State {
    chunks: VecDeque<Chunk>,
    /// The offset of the oldest byte kept (`next` when none is).
    base: u64,
    /// The offset the next byte will have.
    next: u64,
    buffered: usize,
    /// The highest offset a stream has been sent up to.
    delivered: u64,
    /// The program has not been seen to exit yet.
    exit: Option<Exit>,
    exited_at: Option<Instant>,
    /// Answers to signal frames, for the attached stream.
    replies: VecDeque<Vec<u8>>,
    /// Which attach is current; a newer one makes an older one end.
    generation: u64,
    attached: bool,
    detached_at: Instant,
    /// The session was destroyed: nothing more is read or sent.
    ended: bool,
    /// The engine's channel has no more frames.
    finished: bool,
}

struct Entry {
    id: String,
    owner: String,
    /// A console the node itself owns (`console:<source>`): every peer that may use shells attaches to it, nobody's
    /// grant ends it, and it lives until the node ends it.
    console: bool,
    name: Option<String>,
    kind: Kind,
    program: String,
    started: SystemTime,
    on_full: OnFull,
    scrollback: usize,
    linger: Duration,
    /// What the program's kind needs besides `session`.
    needs: Permission,
    state: Mutex<State>,
    changed: Condvar,
    /// The session layer's end of the engine's channel.
    engine: End,
    poll: Duration,
    events: Arc<dyn NodeEvents>,
}

impl Entry {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn peer(&self) -> Option<PeerId> {
        self.owner.parse().ok()
    }

    fn announce(&self, change: SessionChange) {
        if let Some(peer) = self.peer() {
            self.events.event(&NodeEvent::Session {
                peer,
                session: self.id.clone(),
                change,
            });
        }
    }

    /// Takes a piece of output into the buffer, waiting (for `pause`) while the buffer holds output nobody has
    /// taken.
    fn push(&self, stderr: bool, data: Vec<u8>) {
        let mut state = self.lock();
        let length = data.len();
        loop {
            if state.ended {
                return;
            }
            // What a stream has been sent can go, oldest first, when the buffer is over its size.
            while state.buffered + length > self.scrollback
                && state
                    .chunks
                    .front()
                    .is_some_and(|front| front.offset + front.data.len() as u64 <= state.delivered)
            {
                Self::drop_front(&mut state);
            }
            if self.on_full == OnFull::DropOldest
                || state.buffered + length <= self.scrollback
                || state.buffered == 0
            {
                break;
            }
            state = self
                .changed
                .wait_timeout(state, self.poll)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let offset = state.next;
        state.next += length as u64;
        state.buffered += length;
        state.chunks.push_back(Chunk {
            offset,
            stderr,
            data,
        });
        if self.on_full == OnFull::DropOldest {
            while state.buffered > self.scrollback && state.chunks.len() > 1 {
                Self::drop_front(&mut state);
            }
        }
        self.changed.notify_all();
    }

    fn drop_front(state: &mut State) {
        if let Some(front) = state.chunks.pop_front() {
            state.buffered -= front.data.len();
        }
        state.base = state
            .chunks
            .front()
            .map_or(state.next, |chunk| chunk.offset);
    }

    /// Reads the engine's frames until its channel ends.
    fn pump(self: &Arc<Self>) {
        loop {
            match self.engine.receive(Some(self.poll)) {
                Ok(Some(frame)) => self.frame(&frame),
                Ok(None) => break,
                Err(Failure::Timeout) => {
                    if self.lock().ended {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let mut state = self.lock();
        state.finished = true;
        if state.exit.is_none() {
            // The engine ended without an exit frame: the session was destroyed.
            state.ended = true;
        }
        self.changed.notify_all();
    }

    fn frame(&self, frame: &[u8]) {
        match self.kind {
            Kind::Shell => match shell::Output::decode(frame) {
                Ok(shell::Output::Data(bytes)) => self.push(false, bytes),
                Ok(shell::Output::Signalled(_)) => self.reply(frame),
                Ok(shell::Output::Exit(exit)) => self.exited(Exit {
                    code: exit.code,
                    signal: exit.signal,
                    timed_out: exit.timed_out,
                    killed: exit.killed,
                }),
                _ => {}
            },
            _ => match exec::Output::decode(frame) {
                Ok(exec::Output::Stdout(bytes)) => self.push(false, bytes),
                Ok(exec::Output::Stderr(bytes)) => self.push(true, bytes),
                Ok(exec::Output::Signalled(_)) => self.reply(frame),
                Ok(exec::Output::Exit(exit)) => self.exited(exit),
                _ => {}
            },
        }
    }

    fn reply(&self, frame: &[u8]) {
        let mut state = self.lock();
        if state.attached {
            state.replies.push_back(frame.to_vec());
            self.changed.notify_all();
        }
    }

    fn exited(&self, exit: Exit) {
        {
            let mut state = self.lock();
            state.exit = Some(exit);
            state.exited_at = Some(Instant::now());
            self.changed.notify_all();
        }
        self.announce(SessionChange::Ended {
            reason: "exited".to_owned(),
        });
    }

    /// Sends the engine a client frame that is not output: input, a resize, a signal, a kill. `false` when the
    /// engine is gone.
    fn to_engine(&self, frame: &[u8], stop: &AtomicBool) -> bool {
        loop {
            match self.engine.send_timeout(frame, Some(self.poll)) {
                Ok(()) => return true,
                Err(Failure::Timeout) => {
                    if stop.load(Ordering::Acquire) {
                        return false;
                    }
                }
                Err(_) => return false,
            }
        }
    }

    /// Ends the program the way `signal` says (`kill` when none): the session stays, with its exit, until kept
    /// time passes.
    fn end_with(&self, signal: Option<Signal>) {
        let frame = match signal {
            None | Some(Signal::Kill) => vec![KILL_FRAME],
            Some(signal) => exec::Input::Signal(signal).encode(),
        };
        let _ = self.engine.send_timeout(&frame, Some(self.poll));
    }

    /// Ends the session now: the engine's channel closes, which ends its tree, and nothing more is read or sent.
    fn destroy(&self, reason: &str) {
        let announce = {
            let mut state = self.lock();
            let first = !state.ended;
            state.ended = true;
            first && state.exit.is_none()
        };
        self.engine.close();
        self.changed.notify_all();
        if announce {
            self.announce(SessionChange::Ended {
                reason: reason.to_owned(),
            });
        }
    }

    fn listed(&self, with_owner: bool) -> Listed {
        let state = self.lock();
        let since = self
            .started
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        Listed {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind,
            program: self.program.clone(),
            started: Timestamp {
                secs: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
                nanos: since.subsec_nanos(),
            },
            attached: state.attached,
            exit: state.exit,
            next: state.next,
            buffered: state.buffered as u64,
            peer: with_owner.then(|| self.owner.clone()),
        }
    }
}

/// The tag of a `kill` frame in both the exec and the shell protocol.
const KILL_FRAME: u8 = 0x12;
/// The tag of a compressed standard input frame, which a session does not take.
const COMPRESSED_INPUT: u8 = 0x13;

/// The sessions a node holds.
pub struct Sessions {
    config: SessionsConfig,
    grants: Arc<dyn Grants>,
    keys: Arc<dyn Keys>,
    events: Arc<dyn NodeEvents>,
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
    sweeping: AtomicBool,
}

impl core::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sessions")
            .field("held", &lock(&self.entries).len())
            .finish_non_exhaustive()
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        let entries = std::mem::take(&mut *lock(&self.entries));
        for entry in entries.values() {
            entry.destroy("the host stopped");
        }
    }
}

/// The request of a stream that starts a session, once its engine is chosen.
struct Starting {
    kind: Kind,
    program: String,
    needs: Permission,
    request: SessionRequest,
    /// The engine's request: the one the client sent, without compression.
    metadata: Vec<u8>,
    poll: Duration,
    /// The id of a console the node owns, when this starts one.
    console: Option<String>,
}

impl Sessions {
    /// The sessions of a node: `config`'s bounds, checked against `grants` and `keys` every `check_every`.
    pub fn new(
        config: &SessionsConfig,
        grants: Arc<dyn Grants>,
        keys: Arc<dyn Keys>,
        events: Arc<dyn NodeEvents>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config: config.clone(),
            grants,
            keys,
            events,
            entries: Mutex::new(BTreeMap::new()),
            sweeping: AtomicBool::new(false),
        })
    }

    fn id() -> Result<String, ErrorBody> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|_| ErrorBody::new(ErrorCode::Internal, "no randomness for an id"))?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    fn admit(&self, grant: &Grant, peer: &str, needs: Permission) -> Result<(), ErrorBody> {
        if !self.config.enabled {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "this host does not run sessions",
            ));
        }
        if !grant.allows(Permission::Session) {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                "this peer may not start sessions",
            ));
        }
        if !grant.allows(needs) {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                "this peer may not do that",
            ));
        }
        let entries = lock(&self.entries);
        if entries.len() >= self.config.max_detached {
            return Err(busy("the host holds as many sessions as it allows"));
        }
        if entries.values().filter(|entry| entry.owner == peer).count() >= self.config.max_per_peer
        {
            return Err(busy("this peer holds as many sessions as it may"));
        }
        Ok(())
    }

    /// The scrollback and policy a request asks for, within what the host allows.
    fn terms(
        &self,
        request: &SessionRequest,
        default_on_full: OnFull,
    ) -> Result<(usize, OnFull, Duration), ErrorBody> {
        let scrollback = match request.scrollback {
            None => self.config.scrollback,
            Some(0) => {
                return Err(ErrorBody::new(
                    ErrorCode::BadRequest,
                    "a session keeps at least one byte of output",
                ));
            }
            Some(asked) => {
                let asked = usize::try_from(asked).unwrap_or(usize::MAX);
                if asked > self.config.max_scrollback {
                    return Err(ErrorBody::new(
                        ErrorCode::BadRequest,
                        "that is more output than this host keeps for a session",
                    ));
                }
                asked
            }
        };
        let linger = match request.linger_ms {
            None => self.config.linger.0,
            Some(asked) => Duration::from_millis(asked).min(self.config.linger.0),
        };
        Ok((
            scrollback,
            request.on_full.unwrap_or(default_on_full),
            linger,
        ))
    }

    fn register(&self, entry: &Arc<Entry>, per_peer: usize) -> Result<(), ErrorBody> {
        let mut entries = lock(&self.entries);
        if entries.len() >= self.config.max_detached
            || entries
                .values()
                .filter(|held| held.owner == entry.owner)
                .count()
                >= per_peer
        {
            return Err(busy("the host holds as many sessions as it allows"));
        }
        entries.insert(entry.id.clone(), Arc::clone(entry));
        Ok(())
    }

    /// Starts one thread that looks at the sessions every `check_every`, once.
    fn ensure_sweeper(self: &Arc<Self>) {
        if self.sweeping.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak: Weak<Self> = Arc::downgrade(self);
        let every = self.config.check_every.0;
        let _ = thread::Builder::new()
            .name("bohio-session-sweeper".to_owned())
            .spawn(move || {
                loop {
                    thread::sleep(every);
                    let Some(sessions) = weak.upgrade() else {
                        return;
                    };
                    sessions.sweep();
                }
            });
    }

    /// One look at every session: ends and removes what its time or its grant says.
    pub(crate) fn sweep(&self) {
        let now = Instant::now();
        let held: Vec<Arc<Entry>> = lock(&self.entries).values().cloned().collect();
        for entry in held {
            let reason = self.reason_to_end(&entry, now);
            if let Some(reason) = reason {
                entry.destroy(reason);
                lock(&self.entries).remove(&entry.id);
            }
        }
    }

    fn reason_to_end(&self, entry: &Entry, now: Instant) -> Option<&'static str> {
        if entry.console {
            return entry.lock().ended.then_some("ended");
        }
        let covered = entry.peer().is_some_and(|peer| {
            self.keys.is_accepted(&peer)
                && self.grants.grant(&peer).is_ok_and(|grant| {
                    grant.allows(Permission::Session) && grant.allows(entry.needs)
                })
        });
        if !covered {
            return Some("the peer's key or grant no longer covers it");
        }
        let state = entry.lock();
        if state.ended {
            return Some("ended");
        }
        if let Some(at) = state.exited_at {
            return (now.duration_since(at) >= self.config.keep_exited.0)
                .then_some("exited and kept long enough");
        }
        (!state.attached && now.duration_since(state.detached_at) >= entry.linger)
            .then_some("nobody attached within its linger")
    }

    /// The sessions `peer` may see: its own, or every peer's for an `admin`.
    pub(crate) fn list(
        &self,
        peer: &str,
        admin: bool,
        permits: &dyn Fn(&str) -> bool,
    ) -> Vec<Listed> {
        let mut listed: Vec<(SystemTime, Listed)> = lock(&self.entries)
            .values()
            .filter(|entry| {
                admin
                    || entry.owner == peer
                    || (entry.console && entry.id.strip_prefix("console:").is_some_and(permits))
            })
            .map(|entry| (entry.started, entry.listed(admin)))
            .collect();
        listed.sort_by_key(|(started, _)| *started);
        listed.into_iter().map(|(_, listed)| listed).collect()
    }

    /// Ends the session `id` of `peer` (any peer's for an `admin`) with `signal` (`kill` when none).
    ///
    /// # Errors
    ///
    /// `not_found` for an id `peer` may not see.
    pub(crate) fn end(
        &self,
        peer: &str,
        admin: bool,
        id: &str,
        signal: Option<Signal>,
    ) -> Result<(), ErrorBody> {
        let entry = lock(&self.entries)
            .get(id)
            .filter(|entry| admin || entry.owner == peer)
            .cloned()
            .ok_or_else(not_found)?;
        if entry.console {
            // Ending a console is letting go of the source: the engine's terminal is released with its channel.
            entry.destroy("ended by an administrator");
            lock(&self.entries).remove(&entry.id);
        } else {
            entry.end_with(signal);
        }
        Ok(())
    }

    /// Starts the exec session `metadata` asks for, answers `started` on `channel` and serves the stream attached.
    ///
    /// # Errors
    ///
    /// The [`ErrorBody`] sent to the client: `denied` without `session`, `unsupported` when sessions are off or
    /// compression is asked for, `busy` at a limit, `bad_request` for a size the host does not keep, else the
    /// engine's own.
    pub(crate) fn serve_exec(
        self: &Arc<Self>,
        channel: &Arc<dyn Channel>,
        metadata: &[u8],
        peer: &str,
        host: &ExecHost<'_>,
    ) -> Result<Ended, ErrorBody> {
        let refuse = |error: ErrorBody| {
            let _ = channel.send(&encode_error(&error));
            error
        };
        let mut request = exec::Request::from_metadata(metadata).map_err(|_| {
            refuse(ErrorBody::new(
                ErrorCode::BadRequest,
                "the request cannot be read",
            ))
        })?;
        let wanted = request.session.take().ok_or_else(|| {
            refuse(ErrorBody::new(
                ErrorCode::BadRequest,
                "the request asks for no session",
            ))
        })?;
        if request.output_compression.is_some() || request.stdin_compression.is_some() {
            return Err(refuse(ErrorBody::new(
                ErrorCode::Unsupported,
                "a session's streams are not compressed",
            )));
        }
        self.admit(host.grant, peer, Permission::Exec)
            .map_err(refuse)?;
        let (scrollback, on_full, linger) = self.terms(&wanted, OnFull::Pause).map_err(refuse)?;
        let program = match &request.command {
            exec::Command::Argv { argv } => argv.first().cloned().unwrap_or_default(),
            exec::Command::Shell { kind, .. } => kind.program().to_owned(),
            _ => String::new(),
        };
        let starting = Starting {
            kind: Kind::Exec,
            program,
            needs: Permission::Exec,
            request: wanted,
            metadata: request.to_metadata().map_err(|_| {
                refuse(ErrorBody::new(
                    ErrorCode::Internal,
                    "the request cannot be written",
                ))
            })?,
            poll: host.config.poll_interval.0,
            console: None,
        };
        let (mine, theirs) = pair(self.config.queue_frames);
        let engine = {
            let grant = host.grant.clone();
            let config = host.config.clone();
            let compression = host.compression.clone();
            let slots = Arc::clone(host.slots);
            let path = host.path.to_owned();
            let metadata = starting.metadata.clone();
            thread::Builder::new()
                .name("bohio-session-exec".to_owned())
                .spawn(move || {
                    let channel: Arc<dyn Channel> = Arc::new(theirs);
                    let context = crate::exec::Context {
                        grant: &grant,
                        config: &config,
                        compression: &compression,
                        slots: &slots,
                        path: &path,
                    };
                    let _ = crate::exec::serve(Arc::clone(&channel), &metadata, &context);
                    channel.close();
                })
        };
        if engine.is_err() {
            return Err(refuse(ErrorBody::new(
                ErrorCode::Internal,
                "a thread could not be started",
            )));
        }
        self.begin(channel, peer, starting, (scrollback, on_full, linger), mine)
    }

    /// As [`Sessions::serve_exec`], for a shell.
    ///
    /// # Errors
    ///
    /// As [`Sessions::serve_exec`].
    pub(crate) fn serve_shell(
        self: &Arc<Self>,
        channel: &Arc<dyn Channel>,
        metadata: &[u8],
        peer: &str,
        host: &ShellHost<'_>,
    ) -> Result<Ended, ErrorBody> {
        let refuse = |error: ErrorBody| {
            let _ = channel.send(&encode_error_shell(&error));
            error
        };
        let mut request = shell::Request::from_metadata(metadata).map_err(|_| {
            refuse(ErrorBody::new(
                ErrorCode::BadRequest,
                "the request cannot be read",
            ))
        })?;
        let wanted = request.session.take().ok_or_else(|| {
            refuse(ErrorBody::new(
                ErrorCode::BadRequest,
                "the request asks for no session",
            ))
        })?;
        self.admit(host.grant, peer, Permission::Shell)
            .map_err(refuse)?;
        let (scrollback, on_full, linger) =
            self.terms(&wanted, OnFull::DropOldest).map_err(refuse)?;
        let program = match &request.program {
            shell::Program::Kind { kind } => kind.program().to_owned(),
            shell::Program::Argv { argv } => argv.first().cloned().unwrap_or_default(),
            _ => String::new(),
        };
        let starting = Starting {
            kind: Kind::Shell,
            program,
            needs: Permission::Shell,
            request: wanted,
            metadata: request.to_metadata().map_err(|_| {
                refuse(ErrorBody::new(
                    ErrorCode::Internal,
                    "the request cannot be written",
                ))
            })?,
            poll: host.config.poll_interval.0,
            console: None,
        };
        let (mine, theirs) = pair(self.config.queue_frames);
        let engine = {
            let grant = host.grant.clone();
            let config = host.config.clone();
            let slots = Arc::clone(host.slots);
            let path: OsString = host.path.to_owned();
            let sources = Arc::clone(host.sources);
            let metadata = starting.metadata.clone();
            thread::Builder::new()
                .name("bohio-session-shell".to_owned())
                .spawn(move || {
                    let channel: Arc<dyn Channel> = Arc::new(theirs);
                    let context = crate::shell::Context {
                        grant: &grant,
                        config: &config,
                        slots: &slots,
                        path: &path,
                        sources: &sources,
                    };
                    let _ = crate::shell::serve(Arc::clone(&channel), &metadata, &context);
                    channel.close();
                })
        };
        if engine.is_err() {
            return Err(refuse(ErrorBody::new(
                ErrorCode::Internal,
                "a thread could not be started",
            )));
        }
        self.begin(channel, peer, starting, (scrollback, on_full, linger), mine)
    }

    /// Attaches `channel` live to the node's console of a terminal source (`bohio-shell/1` with `Program::Console`),
    /// starting it first when nothing holds it. The console is the node's own session, `console:<source>`: a client's
    /// `kill` or closed stream only detaches, a newer attach supersedes an older, and its output is kept for
    /// `bohio-attach/1` from any offset still held.
    ///
    /// # Errors
    ///
    /// The [`ErrorBody`] sent to the client: `unsupported` when sessions or shells are off or the node serves no such
    /// terminal source, `denied` without `shell`, else the source's own refusal.
    pub(crate) fn serve_console(
        self: &Arc<Self>,
        channel: &Arc<dyn Channel>,
        metadata: &[u8],
        peer: &str,
        host: &ShellHost<'_>,
    ) -> Result<Ended, ErrorBody> {
        let refuse = |error: ErrorBody| {
            let _ = channel.send(&encode_error_shell(&error));
            error
        };
        let request = shell::Request::from_metadata(metadata).map_err(|_| {
            refuse(ErrorBody::new(
                ErrorCode::BadRequest,
                "the request cannot be read",
            ))
        })?;
        let shell::Program::Console { source } = &request.program else {
            return Err(refuse(ErrorBody::new(
                ErrorCode::BadRequest,
                "that is not a console request",
            )));
        };
        if !self.config.enabled || !host.config.enabled {
            return Err(refuse(ErrorBody::new(
                ErrorCode::Unsupported,
                "this host does not run consoles",
            )));
        }
        if !host.grant.allows(Permission::Shell) {
            return Err(refuse(ErrorBody::new(
                ErrorCode::Denied,
                "this peer may not open shells",
            )));
        }
        let named = host.sources.terminal(source.as_deref()).map_err(refuse)?;
        if !host.sources.permits(host.grant, named.name()) {
            return Err(refuse(ErrorBody::new(
                ErrorCode::Denied,
                "this peer may not use that terminal source",
            )));
        }
        let id = format!("console:{}", named.name());
        let held = lock(&self.entries)
            .get(&id)
            .filter(|entry| !entry.lock().ended)
            .cloned();
        if let Some(entry) = held {
            let started = shell::Output::Started(shell::Started {
                pid: None,
                session: Some(id),
            })
            .encode()
            .unwrap_or_default();
            if !send_frame(channel, &started, entry.poll, &AtomicBool::new(false)) {
                return Ok(Ended::Gone);
            }
            let from = entry.lock().next;
            return Ok(self.attached(&entry, channel, from));
        }
        let wanted = SessionRequest::default();
        let (scrollback, on_full, linger) =
            self.terms(&wanted, OnFull::DropOldest).map_err(refuse)?;
        let starting = Starting {
            kind: Kind::Shell,
            program: id.clone(),
            needs: Permission::Shell,
            request: wanted,
            metadata: request.to_metadata().map_err(|_| {
                refuse(ErrorBody::new(
                    ErrorCode::Internal,
                    "the request cannot be written",
                ))
            })?,
            poll: host.config.poll_interval.0,
            console: Some(id),
        };
        let (mine, theirs) = pair(self.config.queue_frames);
        let engine = {
            let grant = host.grant.clone();
            let config = host.config.clone();
            let slots = Arc::clone(host.slots);
            let path: OsString = host.path.to_owned();
            let sources = Arc::clone(host.sources);
            let metadata = starting.metadata.clone();
            thread::Builder::new()
                .name("bohio-session-console".to_owned())
                .spawn(move || {
                    let channel: Arc<dyn Channel> = Arc::new(theirs);
                    let context = crate::shell::Context {
                        grant: &grant,
                        config: &config,
                        slots: &slots,
                        path: &path,
                        sources: &sources,
                    };
                    let _ = crate::shell::serve(Arc::clone(&channel), &metadata, &context);
                    channel.close();
                })
        };
        if engine.is_err() {
            return Err(refuse(ErrorBody::new(
                ErrorCode::Internal,
                "a thread could not be started",
            )));
        }
        self.begin(channel, peer, starting, (scrollback, on_full, linger), mine)
    }

    /// Waits for the engine's first frame, then registers the session and serves `channel` attached to it.
    fn begin(
        self: &Arc<Self>,
        channel: &Arc<dyn Channel>,
        peer: &str,
        starting: Starting,
        terms: (usize, OnFull, Duration),
        engine: End,
    ) -> Result<Ended, ErrorBody> {
        let (scrollback, on_full, linger) = terms;
        // The engine answers `started` or an error as its first frame, and ends its channel after the error.
        let first = loop {
            match engine.receive(Some(starting.poll)) {
                Ok(Some(frame)) => break frame,
                Err(Failure::Timeout) if !channel.is_closed() => {}
                _ => {
                    engine.close();
                    return Err(ErrorBody::new(
                        ErrorCode::Internal,
                        "the session did not start",
                    ));
                }
            }
        };
        let fresh_id = || match &starting.console {
            Some(id) => Ok(id.clone()),
            None => Self::id(),
        };
        let (pid, id) = match starting.kind {
            Kind::Shell => match shell::Output::decode(&first) {
                Ok(shell::Output::Started(started)) => (started.pid, fresh_id()?),
                Ok(shell::Output::Error(error)) => {
                    let _ = channel.send(&first);
                    return Err(error);
                }
                _ => {
                    return Err(ErrorBody::new(
                        ErrorCode::Internal,
                        "the session did not start",
                    ));
                }
            },
            _ => match exec::Output::decode(&first) {
                Ok(exec::Output::Started(started)) => (Some(started.pid), Self::id()?),
                Ok(exec::Output::Error(error)) => {
                    let _ = channel.send(&first);
                    return Err(error);
                }
                _ => {
                    return Err(ErrorBody::new(
                        ErrorCode::Internal,
                        "the session did not start",
                    ));
                }
            },
        };
        let entry = Arc::new(Entry {
            id: id.clone(),
            owner: if starting.console.is_some() {
                "node".to_owned()
            } else {
                peer.to_owned()
            },
            console: starting.console.is_some(),
            name: starting.request.name.clone(),
            kind: starting.kind,
            program: starting.program,
            started: SystemTime::now(),
            on_full,
            scrollback,
            linger,
            needs: starting.needs,
            state: Mutex::new(State {
                chunks: VecDeque::new(),
                base: 0,
                next: 0,
                buffered: 0,
                delivered: 0,
                exit: None,
                exited_at: None,
                replies: VecDeque::new(),
                generation: 0,
                attached: false,
                detached_at: Instant::now(),
                ended: false,
                finished: false,
            }),
            changed: Condvar::new(),
            engine,
            poll: starting.poll,
            events: Arc::clone(&self.events),
        });
        if let Err(error) = self.register(&entry, self.config.max_per_peer) {
            entry.destroy("the host is full");
            let _ = channel.send(&match starting.kind {
                Kind::Shell => encode_error_shell(&error),
                _ => encode_error(&error),
            });
            return Err(error);
        }
        self.ensure_sweeper();
        let for_pump = Arc::clone(&entry);
        let pumped = thread::Builder::new()
            .name("bohio-session-pump".to_owned())
            .spawn(move || for_pump.pump());
        if pumped.is_err() {
            entry.destroy("a thread could not be started");
            lock(&self.entries).remove(&id);
            return Err(ErrorBody::new(
                ErrorCode::Internal,
                "a thread could not be started",
            ));
        }
        entry.announce(SessionChange::Started);
        // Tell the client the program started, with the session's id in it.
        let started = match starting.kind {
            Kind::Shell => shell::Output::Started(shell::Started {
                pid,
                session: Some(id),
            })
            .encode(),
            _ => exec::Output::Started(exec::Started {
                session: Some(id),
                ..exec::Started::new(pid.unwrap_or(0))
            })
            .encode(),
        };
        let never = AtomicBool::new(false);
        let sent = started
            .ok()
            .is_some_and(|frame| send_frame(channel, &frame, starting.poll, &never));
        if sent && starting.request.detach {
            // Nothing is sent to a stream whose reader only wants the id.
            entry.lock().detached_at = Instant::now();
            return Ok(Ended::Done);
        }
        if !sent {
            // The client is gone before it heard the id: the session is detached and nobody knows its id, which is
            // what lingering ends.
            entry.lock().detached_at = Instant::now();
            return Ok(Ended::Gone);
        }
        Ok(self.attached(&entry, channel, 0))
    }

    /// Serves one `bohio-attach/1` stream: answers `attached` and then the session's frames from the offset asked for.
    ///
    /// # Errors
    ///
    /// `not_found` for an id the peer does not hold, `bad_request` for an offset not written yet, `denied` without
    /// `session`.
    pub(crate) fn serve_attach(
        self: &Arc<Self>,
        channel: &Arc<dyn Channel>,
        metadata: &[u8],
        peer: &str,
        grant: &Grant,
    ) -> Result<Ended, ErrorBody> {
        let refuse = |error: ErrorBody| {
            // Both protocols give an error frame the same tag.
            let _ = channel.send(&encode_error(&error));
            error
        };
        let request = Attach::from_metadata(metadata).map_err(|_| {
            refuse(ErrorBody::new(
                ErrorCode::BadRequest,
                "the request cannot be read",
            ))
        })?;
        if !self.config.enabled {
            return Err(refuse(ErrorBody::new(
                ErrorCode::Unsupported,
                "this host does not run sessions",
            )));
        }
        if !grant.allows(Permission::Session) {
            return Err(refuse(ErrorBody::new(
                ErrorCode::Denied,
                "this peer may not use sessions",
            )));
        }
        let entry = lock(&self.entries)
            .get(&request.session)
            .filter(|entry| entry.owner == peer || entry.console)
            .cloned()
            .ok_or_else(|| refuse(not_found()))?;
        if !grant.allows(entry.needs) {
            return Err(refuse(not_found()));
        }
        if let Some(source) = entry.id.strip_prefix("console:")
            && !grant.permits_source(source)
        {
            return Err(refuse(not_found()));
        }
        let (from, lost) = {
            let state = entry.lock();
            if state.ended {
                drop(state);
                return Err(refuse(not_found()));
            }
            match request.from {
                None => (state.next, 0),
                Some(asked) if asked > state.next => {
                    drop(state);
                    return Err(refuse(ErrorBody::new(
                        ErrorCode::BadRequest,
                        "that offset has not been written yet",
                    )));
                }
                Some(asked) if asked < state.base => (state.base, state.base - asked),
                Some(asked) => (asked, 0),
            }
        };
        let attached = Attached {
            kind: entry.kind,
            from,
            lost,
        };
        let frame = attached
            .encode()
            .map_err(|_| ErrorBody::new(ErrorCode::Internal, "the answer cannot be written"))?;
        if !send_frame(channel, &frame, entry.poll, &AtomicBool::new(false)) {
            return Ok(Ended::Gone);
        }
        Ok(self.attached(&entry, channel, from))
    }

    /// Feeds `channel` from the session's buffer, starting at `from`, and passes its frames to the engine, until the
    /// stream ends, another attaches or the program's exit has been delivered.
    fn attached(&self, entry: &Arc<Entry>, channel: &Arc<dyn Channel>, from: u64) -> Ended {
        let generation = {
            let mut state = entry.lock();
            state.generation += 1;
            state.attached = true;
            state.replies.clear();
            entry.changed.notify_all();
            state.generation
        };
        entry.announce(SessionChange::Attached);
        let stop = Arc::new(AtomicBool::new(false));
        let gone = Arc::new(AtomicBool::new(false));
        let receiver = {
            let (entry, channel, stop, gone) = (
                Arc::clone(entry),
                Arc::clone(channel),
                Arc::clone(&stop),
                Arc::clone(&gone),
            );
            thread::Builder::new()
                .name("bohio-session-input".to_owned())
                .spawn(move || forward_input(&entry, &channel, &stop, &gone))
        };
        let ended = if receiver.is_err() {
            Ended::Gone
        } else {
            self.feed(entry, channel, from, generation, &gone)
        };
        stop.store(true, Ordering::Release);
        {
            let mut state = entry.lock();
            // A newer attach owns these flags now.
            if state.generation == generation {
                state.attached = false;
                state.detached_at = Instant::now();
            }
            entry.changed.notify_all();
        }
        if ended != Ended::Superseded {
            entry.announce(SessionChange::Detached);
        }
        ended
    }

    fn feed(
        &self,
        entry: &Arc<Entry>,
        channel: &Arc<dyn Channel>,
        from: u64,
        generation: u64,
        gone: &AtomicBool,
    ) -> Ended {
        let max = channel
            .capabilities()
            .max_send
            .map_or(usize::MAX, |max| max.saturating_sub(1))
            .max(1);
        let mut cursor = from;
        loop {
            let mut frames: Vec<Vec<u8>> = Vec::new();
            let mut last = false;
            {
                let mut state = entry.lock();
                loop {
                    if state.generation != generation {
                        return Ended::Superseded;
                    }
                    if gone.load(Ordering::Acquire) || channel.is_closed() {
                        return Ended::Gone;
                    }
                    if state.ended {
                        return Ended::Done;
                    }
                    cursor = cursor.max(state.base);
                    frames.extend(state.replies.drain(..));
                    let from = cursor;
                    for chunk in state
                        .chunks
                        .iter()
                        .filter(|chunk| chunk.offset + chunk.data.len() as u64 > from)
                    {
                        let skip =
                            usize::try_from(cursor.saturating_sub(chunk.offset)).unwrap_or(0);
                        let data = &chunk.data[skip..];
                        for piece in data.chunks(max.saturating_sub(1).max(1)) {
                            frames.push(entry.output_frame(chunk.stderr, piece));
                        }
                        cursor = chunk.offset + chunk.data.len() as u64;
                        // A pass gathers at most `queue_frames` frames, so the copy it makes is bounded by a setting.
                        if frames.len() >= self.config.queue_frames {
                            break;
                        }
                    }
                    if cursor >= state.next
                        && let Some(exit) = state.exit
                        && state.finished
                    {
                        // Every byte is before the exit: the exit is the last frame.
                        frames.push(entry.exit_frame(exit));
                        last = true;
                    }
                    if !frames.is_empty() {
                        break;
                    }
                    state = entry
                        .changed
                        .wait_timeout(state, entry.poll)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
            for frame in &frames {
                if !send_frame(channel, frame, entry.poll, gone) {
                    return Ended::Gone;
                }
            }
            {
                let mut state = entry.lock();
                state.delivered = state.delivered.max(cursor);
                entry.changed.notify_all();
            }
            if last {
                return Ended::Done;
            }
        }
    }
}

impl Entry {
    fn output_frame(&self, stderr: bool, bytes: &[u8]) -> Vec<u8> {
        match self.kind {
            Kind::Shell => shell::Output::Data(bytes.to_vec())
                .encode()
                .unwrap_or_default(),
            _ if stderr => exec::Output::Stderr(bytes.to_vec())
                .encode()
                .unwrap_or_default(),
            _ => exec::Output::Stdout(bytes.to_vec())
                .encode()
                .unwrap_or_default(),
        }
    }

    fn exit_frame(&self, exit: Exit) -> Vec<u8> {
        match self.kind {
            Kind::Shell => shell::Output::Exit(shell::Exit {
                code: exit.code,
                signal: exit.signal,
                timed_out: exit.timed_out,
                killed: exit.killed,
            })
            .encode()
            .unwrap_or_default(),
            _ => exec::Output::Exit(exit).encode().unwrap_or_default(),
        }
    }
}

/// Sends one frame, waiting for room, until the stream ends.
fn send_frame(channel: &Arc<dyn Channel>, frame: &[u8], poll: Duration, gone: &AtomicBool) -> bool {
    loop {
        match channel.send_timeout(frame, Some(poll)) {
            Ok(()) => return true,
            Err(Failure::Timeout | Failure::NoProgress) => {
                if gone.load(Ordering::Acquire) || channel.is_closed() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

/// Passes the attached client's frames to the engine until the stream ends or the attach does.
fn forward_input(
    entry: &Arc<Entry>,
    channel: &Arc<dyn Channel>,
    stop: &AtomicBool,
    gone: &AtomicBool,
) {
    while !stop.load(Ordering::Acquire) {
        match channel.receive(Some(entry.poll)) {
            Ok(Some(frame)) => {
                // Compressed input needs a decoder the session does not make.
                if frame.first() == Some(&COMPRESSED_INPUT) {
                    continue;
                }
                if !entry.to_engine(&frame, stop) {
                    return;
                }
            }
            Err(Failure::Timeout) => {}
            Ok(None) | Err(_) => {
                gone.store(true, Ordering::Release);
                entry.changed.notify_all();
                return;
            }
        }
    }
}

/// An error frame on an exec or attach stream.
fn encode_error(error: &ErrorBody) -> Vec<u8> {
    exec::Output::Error(error.clone())
        .encode()
        .unwrap_or_else(|_| vec![0x05])
}

/// An error frame on a shell stream.
fn encode_error_shell(error: &ErrorBody) -> Vec<u8> {
    shell::Output::Error(error.clone())
        .encode()
        .unwrap_or_else(|_| vec![0x05])
}

/// Whether `metadata` asks for a session: the request of an exec or a shell with a `session`.
pub(crate) fn wants_exec(metadata: &[u8]) -> bool {
    exec::Request::from_metadata(metadata).is_ok_and(|request| request.session.is_some())
}

/// As [`wants_exec`], for a shell request.
pub(crate) fn wants_console(metadata: &[u8]) -> bool {
    shell::Request::from_metadata(metadata)
        .is_ok_and(|request| matches!(request.program, shell::Program::Console { .. }))
}

/// Whether a `bohio-shell/1` open asks to start a session.
pub(crate) fn wants_shell(metadata: &[u8]) -> bool {
    shell::Request::from_metadata(metadata).is_ok_and(|request| request.session.is_some())
}

/// Whether `grant` still covers what an attach stream asked for: holding sessions at all.
pub(crate) fn covers_attach(grant: &Grant, metadata: &[u8]) -> bool {
    grant.allows(Permission::Session)
        && Attach::from_metadata(metadata).map_or(true, |request| {
            request
                .session
                .strip_prefix("console:")
                .is_none_or(|source| grant.permits_source(source))
        })
}
