//! Interactive shells: serves one `bohio-shell/1` stream on a terminal source.
//!
//! [`serve`] checks the peer's grant, asks the node's terminal source ([`crate::access::Sources`]) to open a terminal
//! on a shell (or an explicit program), streams what the terminal produces as it is written, applies the client's
//! typing, resizes and kill requests, and always ends the whole tree: when the stream closes, on a kill request and
//! after the program exits. It is separate from `bohio-exec/1`: a terminal merges the program's output and error and
//! renders them, and takes typed input. Which source opens the terminal, and how it starts a program, is the source's.

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio_api::guatiao::library::Object;
use bohio_api::model::{EnvVar, ProgramSpec, TerminalExit, TerminalOpen};
use bohio_api::{Terminal, TerminalSource};
use bohio_proto::exec::Env;
use bohio_proto::shell::{
    Exit, Input, MAX_DIMENSION, Output, Program, Request, ShellKind, Signal, Signalled, Started,
};
use bohio_proto::{ErrorBody, ErrorCode};
use coqui_transport::{Channel, Failure};

use crate::access::{Source, Sources, code_of};
use crate::config::ShellConfig;
use crate::limit::{Permit, Slots};
use crate::policy::{Grant, Permission};
use crate::stream::{Unsent, chunk_size, send_until};

/// What [`serve`] needs besides the stream.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// What this peer may do.
    pub grant: &'a Grant,
    /// The `[shell]` settings.
    pub config: &'a ShellConfig,
    /// The bound on concurrent sessions, shared by every stream.
    pub slots: &'a Slots,
    /// The peer's id as text: the peer's share of the slots is counted by it.
    pub peer: &'a str,
    /// The host's `PATH`, where a shell kind or a program without a directory is looked for.
    pub path: &'a OsStr,
    /// The node's sources: the default terminal one opens the terminal.
    pub sources: &'a Sources,
}

/// What the engine drives: the open terminal and the source it came from.
struct Open {
    terminal: Object<dyn Terminal>,
    source: Source<dyn TerminalSource>,
    /// The request was for the node's console: a client's `kill` only lets go, the source is never ended by a peer.
    console: bool,
}

impl Open {
    /// Ends the program and its tree, and says how it ended.
    fn kill(&self) -> Exit {
        self.terminal
            .kill()
            .map(|exit| exit_of(&exit))
            .unwrap_or_default()
    }

    /// How the program ended, `None` while it runs; `Err` when it cannot be looked at, or the source was reconfigured
    /// and what ran on it ends.
    fn exited(&self) -> Result<Option<TerminalExit>, ()> {
        if !self.source.is_current() {
            return Err(());
        }
        self.terminal.exit().map_err(|_| ())
    }
}

fn exit_of(exit: &TerminalExit) -> Exit {
    Exit {
        code: exit.code.and_then(|code| i32::try_from(code).ok()),
        signal: exit.signal.and_then(|signal| i32::try_from(signal).ok()),
        ..Exit::default()
    }
}

/// Serves one `bohio-shell/1` stream whose open carried `metadata`, until the program has ended
/// and its last frame is sent.
///
/// Frames out: `started`, then `data` frames as the terminal produces output, then `exit` after
/// every output byte. A request that cannot run gets one `error` frame and the same error is
/// returned; the stream is left open for the caller to close. A stream that closes before the
/// program ends ends its tree; the result is then the program's exit with `killed` set. A resize
/// to a size outside `1..=MAX_DIMENSION` is ignored.
///
/// # Errors
///
/// The [`ErrorBody`] sent to the peer: `bad_request` for an undecodable request, an empty or
/// impossible program, a kind this host does not know or a size outside `1..=MAX_DIMENSION`;
/// `unsupported` when shells are disabled or the node serves no terminal source; `denied` for a peer without `shell` or an explicit
/// program its grant's program list does not hold; `not_found` for a shell the host lacks, a
/// program or a working directory that does not exist; `busy` at `max_sessions`; `io` for other
/// start failures.
pub fn serve(
    channel: Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<Exit, ErrorBody> {
    let shared = Arc::new(Shared::new(channel, context.config.poll_interval.0));
    let (open, _permit, timeout) = match start(metadata, context) {
        Ok(started) => started,
        Err(error) => {
            let _ = shared.send(&encode(&Output::Error(error.clone())));
            return Err(error);
        }
    };
    let open = Arc::new(open);
    let pid = open
        .terminal
        .pid()
        .ok()
        .flatten()
        .and_then(|pid| u32::try_from(pid).ok())
        .unwrap_or(0);
    tracing::debug!(pid, "shell started");

    if !shared.send(&encode(&Output::Started(Started::new(pid)))) {
        let _ = open.kill();
        return Ok(Exit {
            killed: true,
            ..Exit::default()
        });
    }

    let chunk = chunk_size(shared.channel.as_ref(), context.config.chunk_size);
    let spawned = {
        let shared = Arc::clone(&shared);
        let open = Arc::clone(&open);
        thread::Builder::new()
            .name("bohio-shell-reader".to_owned())
            .spawn(move || read_output(&shared, &open, chunk))
    };
    if spawned.is_err() {
        return Err(abandon(&shared, &open));
    }
    let monitor = {
        let shared = Arc::clone(&shared);
        let open = Arc::clone(&open);
        let drain_quiet = context.config.drain_quiet.0;
        let kill_wait = context.config.kill_wait.0;
        thread::Builder::new()
            .name("bohio-shell-monitor".to_owned())
            .spawn(move || monitor(&shared, &open, drain_quiet, kill_wait, timeout))
    };
    let Ok(monitor) = monitor else {
        return Err(abandon(&shared, &open));
    };

    receive_frames(&shared, &open);
    Ok(finish(monitor, &open))
}

/// Whether `grant` still allows the request a `bohio-shell/1` stream opened with: an explicit
/// program is one the program list holds (a shell kind needs only `shell`). A request that cannot be
/// read is covered, so [`serve`] answers it.
pub(crate) fn covers(grant: &Grant, metadata: &[u8]) -> bool {
    Request::from_metadata(metadata).map_or(true, |request| match &request.program {
        Program::Console {
            source: Some(source),
        } => grant.permits_source(source),
        Program::Argv { argv } => {
            argv.first()
                .is_none_or(|program| grant.permits_program(program))
                && grant.permits_env(&request.env)
        }
        _ => true,
    })
}

/// Ends a session whose threads could not be started.
fn abandon(shared: &Shared, open: &Open) -> ErrorBody {
    let _ = open.kill();
    let error = ErrorBody::new(ErrorCode::Internal, "a thread could not be started");
    let _ = shared.send(&encode(&Output::Error(error.clone())));
    error
}

/// State the session's threads share.
struct Shared {
    channel: Arc<dyn Channel>,
    poll: Duration,
    /// The stream ended or failed: nothing more will be read or delivered.
    gone: AtomicBool,
    /// The last frame is sent: a sender waiting for room can stop.
    done: AtomicBool,
    /// The host ended the tree on purpose (kill frame or closed stream).
    killed: AtomicBool,
    /// The client asked to end the tree.
    kill_asked: AtomicBool,
    /// The output reader has finished: every byte it will ever read is sent.
    reader_done: AtomicBool,
    /// The output reader is waiting for the stream to take a frame.
    sending: AtomicBool,
    /// When the terminal last produced output.
    last_output: Mutex<Instant>,
}

impl Shared {
    fn new(channel: Arc<dyn Channel>, poll: Duration) -> Self {
        Self {
            channel,
            poll,
            gone: AtomicBool::new(false),
            done: AtomicBool::new(false),
            killed: AtomicBool::new(false),
            kill_asked: AtomicBool::new(false),
            reader_done: AtomicBool::new(false),
            sending: AtomicBool::new(false),
            last_output: Mutex::new(Instant::now()),
        }
    }

    /// Sends one message, waiting while the stream has no room. `false` when the stream is gone
    /// or the exchange is over, and the message was not sent.
    fn send(&self, message: &[u8]) -> bool {
        let over = || self.gone.load(Ordering::Acquire) || self.done.load(Ordering::Acquire);
        match send_until(self.channel.as_ref(), message, self.poll, &over) {
            Ok(()) => true,
            Err(Unsent::Stopped) => false,
            Err(Unsent::Failed) => {
                self.gone.store(true, Ordering::Release);
                false
            }
        }
    }

    fn touch(&self) {
        *self
            .last_output
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn quiet_for(&self) -> Duration {
        self.last_output
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }
}

fn encode(output: &Output) -> Vec<u8> {
    output.encode().unwrap_or_else(|_| {
        // Every body the engine builds encodes; an empty error frame is still a frame.
        vec![0x05]
    })
}

fn refuse(code: ErrorCode, message: &'static str) -> ErrorBody {
    ErrorBody::new(code, message)
}

/// Checks the request, asks the terminal source to open it, and returns the slot it holds.
fn start<'a>(
    metadata: &[u8],
    context: &Context<'a>,
) -> Result<(Open, Permit<'a>, Option<Duration>), ErrorBody> {
    let request = Request::from_metadata(metadata)
        .map_err(|_| refuse(ErrorCode::BadRequest, "the request cannot be read"))?;
    if !context.config.enabled {
        return Err(refuse(
            ErrorCode::Unsupported,
            "this host does not open shells",
        ));
    }
    if !context.grant.allows(Permission::Shell) {
        return Err(refuse(ErrorCode::Denied, "this peer may not open shells"));
    }
    let console = matches!(request.program, Program::Console { .. });
    let named = match &request.program {
        Program::Console { source } => source.as_deref(),
        _ => None,
    };
    let source = context.sources.terminal(named)?;
    if !context.sources.permits(context.grant, source.name()) {
        return Err(refuse(
            ErrorCode::Denied,
            "this peer may not use that terminal source",
        ));
    }
    crate::access::refuse_display_forward(context.grant, source.get().describe())?;
    let program = resolve(&request.program, &request.env, context)?;
    if !console
        && let Ok(described) = source.get().describe()
        && described.lacks(bohio_api::vocab::terminal::PROGRAMS)
    {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "this node's terminal is the {} source {}; it starts no program",
                described.provider,
                source.name()
            ),
        ));
    }
    let sized = |n: u16| (1..=MAX_DIMENSION).contains(&n);
    if !sized(request.cols) || !sized(request.rows) {
        return Err(refuse(
            ErrorCode::BadRequest,
            "a terminal is 1 to 32767 columns and rows",
        ));
    }
    let permit = context.slots.acquire_for(context.peer).ok_or_else(|| {
        refuse(
            ErrorCode::Busy,
            "the host is running as many shell sessions as it allows",
        )
    })?;
    let env: &Env = &request.env;
    let mut changes: Vec<EnvVar> = env
        .set
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.clone(),
            value: Some(value.clone()),
        })
        .collect();
    changes.extend(env.remove.iter().map(|name| EnvVar {
        name: name.clone(),
        value: None,
    }));
    let open = TerminalOpen {
        program,
        cwd: request.cwd.clone(),
        clear_env: env.clear,
        env: changes,
        path: Some(context.path.to_string_lossy().into_owned()),
        on_host_death: Some(context.config.host_death.name().to_owned()),
        cols: request.cols,
        rows: request.rows,
    };
    let terminal = source
        .get()
        .open(open)
        .map_err(|error| ErrorBody::new(code_of(error.status), error.message().to_string()))?;
    Ok((
        Open {
            terminal,
            source,
            console,
        },
        permit,
        request.timeout_ms.map(Duration::from_millis),
    ))
}

/// The program a request names, checked against the grant. Finding it on the host is the source's.
fn resolve(
    program: &Program,
    env: &Env,
    context: &Context<'_>,
) -> Result<Option<ProgramSpec>, ErrorBody> {
    match program {
        // A console starts no program: whatever the source serves is what the peer gets.
        Program::Console { .. } => Ok(None),
        Program::Kind { kind } => {
            if *kind == ShellKind::Other {
                return Err(refuse(
                    ErrorCode::BadRequest,
                    "this host does not know that kind of shell",
                ));
            }
            Ok(Some(ProgramSpec {
                kind: Some(kind.program().to_owned()),
                ..ProgramSpec::default()
            }))
        }
        Program::Argv { argv } => {
            let Some(first) = argv.first() else {
                return Err(refuse(ErrorCode::BadRequest, "the program is empty"));
            };
            if first.is_empty() || argv.iter().any(|word| word.contains('\0')) {
                return Err(refuse(
                    ErrorCode::BadRequest,
                    "the program is empty or holds a NUL",
                ));
            }
            // Under a program list only the listed programs, named as listed, may start: the
            // list is how an administrator keeps a peer from running anything else.
            if !context.grant.permits_program(first) {
                return Err(refuse(
                    ErrorCode::Denied,
                    "this peer may not run that program",
                ));
            }
            // Under a list the program is the one the host finds at the listed name on its own
            // `PATH`, and the request changes only the variables the grant names.
            let listed = context.grant.restricts_programs();
            if listed && !context.grant.permits_env(env) {
                return Err(refuse(
                    ErrorCode::Denied,
                    "under a program list a peer may change only the variables its grant names, and may not clear the environment",
                ));
            }
            Ok(Some(ProgramSpec {
                kind: None,
                argv: argv.clone(),
                listed,
            }))
        }
        _ => Err(refuse(
            ErrorCode::Unsupported,
            "this host does not know that way to name a program",
        )),
    }
}

/// Copies the terminal's output to the stream in frames until it ends, then reports so.
fn read_output(shared: &Shared, open: &Open, chunk: usize) {
    let mut buffer = vec![0_u8; chunk];
    let wait = u64::try_from(shared.poll.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    loop {
        if shared.gone.load(Ordering::Acquire) || shared.done.load(Ordering::Acquire) {
            break;
        }
        match open.terminal.read(&mut buffer, wait) {
            Ok(0) => {}
            Ok(count) if count > 0 => {
                let count = usize::try_from(count).unwrap_or(0).min(buffer.len());
                shared.touch();
                if !deliver(shared, buffer[..count].to_vec()) {
                    break;
                }
            }
            // The terminal ended for good, or its device is gone.
            Ok(_) | Err(_) => break,
        }
    }
    shared.reader_done.store(true, Ordering::Release);
}

/// Sends one chunk of output, saying while it waits that the reader is not idle.
fn deliver(shared: &Shared, output: Vec<u8>) -> bool {
    shared.sending.store(true, Ordering::Release);
    let sent = shared.send(&encode(&Output::Data(output)));
    shared.sending.store(false, Ordering::Release);
    sent
}

/// Watches the program until it has ended and its output is sent, then sends the last frame.
fn monitor(
    shared: &Shared,
    open: &Open,
    drain_quiet: Duration,
    kill_wait: Duration,
    timeout: Option<Duration>,
) -> Exit {
    let started = Instant::now();
    let mut timed_out = false;
    let natural = loop {
        if shared.gone.load(Ordering::Acquire) || shared.channel.is_closed() {
            shared.gone.store(true, Ordering::Release);
            shared.killed.store(true, Ordering::Release);
            break false;
        }
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            timed_out = true;
            shared.killed.store(true, Ordering::Release);
            break false;
        }
        if shared.kill_asked.load(Ordering::Acquire) {
            shared.killed.store(true, Ordering::Release);
            break false;
        }
        match open.exited() {
            Ok(Some(_)) => break true,
            Ok(None) => thread::sleep(shared.poll),
            Err(()) => {
                shared.killed.store(true, Ordering::Release);
                break false;
            }
        }
    };

    // The program has exited by itself: its terminal may still deliver the last of its output, so
    // wait until it is quiet (or ended) before ending the rest of the tree. A reader that is slow to take
    // the output does not extend this, and neither does a descendant that keeps writing past `timeout`:
    // only the exit waits for the reader, after the tree is ended.
    if natural {
        while !shared.reader_done.load(Ordering::Acquire)
            && !shared.gone.load(Ordering::Acquire)
            && shared.quiet_for() < drain_quiet
            && !timeout.is_some_and(|limit| started.elapsed() >= limit)
        {
            thread::sleep(shared.poll);
        }
    }
    let ended = open.kill();

    // The tree is gone, so the terminal's output ends; a reader still waiting for the stream to
    // take a frame is given all the time it needs, one that is not is given `kill_wait`.
    let settle = Instant::now() + kill_wait;
    while !shared.reader_done.load(Ordering::Acquire) && !shared.gone.load(Ordering::Acquire) {
        if !shared.sending.load(Ordering::Acquire) && Instant::now() >= settle {
            break;
        }
        thread::sleep(shared.poll);
    }

    let exit = Exit {
        code: ended.code,
        signal: ended.signal,
        timed_out,
        killed: shared.killed.load(Ordering::Acquire),
    };
    shared.send(&encode(&Output::Exit(exit)));
    shared.done.store(true, Ordering::Release);
    exit
}

/// Applies the client's frames until the exchange is over or the stream ends.
fn receive_frames(shared: &Shared, open: &Open) {
    while !shared.done.load(Ordering::Acquire) {
        match shared.channel.receive(Some(shared.poll)) {
            Ok(Some(frame)) => match Input::decode(&frame) {
                Ok(Input::Data(bytes)) => {
                    // A failed write means the program is gone; the monitor sees that.
                    let _ = open.terminal.write(&bytes);
                }
                Ok(Input::Resize(size)) => {
                    let sized = |n: u16| (1..=MAX_DIMENSION).contains(&n);
                    if sized(size.cols) && sized(size.rows) {
                        let _ = open.terminal.resize(size.cols, size.rows);
                    }
                }
                // A console is the source's, not the peer's: asking to end it only detaches.
                Ok(Input::Kill | Input::Signal(Signal::Kill)) if open.console => {}
                Ok(Input::Kill | Input::Signal(Signal::Kill)) => {
                    shared.kill_asked.store(true, Ordering::Release);
                    if let Ok(Input::Signal(signal)) = Input::decode(&frame) {
                        let _ = shared.send(&encode(&Output::Signalled(Signalled {
                            signal,
                            delivered: true,
                            message: None,
                        })));
                    }
                }
                Ok(Input::Signal(signal)) => {
                    let delivery = open.terminal.signal(signal.name());
                    let signalled = match delivery {
                        Ok(delivery) => {
                            if delivery.ended {
                                shared.killed.store(true, Ordering::Release);
                            }
                            Signalled {
                                signal,
                                delivered: delivery.delivered,
                                message: delivery.message,
                            }
                        }
                        Err(error) => Signalled {
                            signal,
                            delivered: false,
                            message: Some(error.message().to_string()),
                        },
                    };
                    let _ = shared.send(&encode(&Output::Signalled(signalled)));
                }
                // A frame this version does not know, or a body that is not what its tag says,
                // is skipped: the session goes on.
                Ok(_) | Err(_) => {}
            },
            Err(Failure::Timeout) => {}
            Ok(None) | Err(_) => {
                shared.gone.store(true, Ordering::Release);
                return;
            }
        }
    }
}

/// Waits for the monitor, which always ends once the program is gone.
fn finish(monitor: JoinHandle<Exit>, open: &Open) -> Exit {
    monitor.join().unwrap_or_else(|_| Exit {
        killed: true,
        ..open.kill()
    })
}
