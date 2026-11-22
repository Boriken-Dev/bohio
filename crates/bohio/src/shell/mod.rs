//! Interactive shells: serves one `bohio-shell/1` stream on a pseudo-terminal.
//!
//! [`serve`] checks the peer's grant, starts a shell (or an explicit program) on a terminal in a
//! tree of its own, streams what the terminal produces as it is written, applies the client's
//! typing, resizes and kill requests, and always ends the whole tree: when the stream closes, on
//! a kill request and after the program exits. It is separate from `bohio-exec/1`: a terminal
//! merges the program's output and error and renders them, and takes typed input.

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio_proto::exec::Env;
use bohio_proto::shell::{
    Exit, Input, MAX_DIMENSION, Output, Program, Request, ShellKind, Started,
};
use bohio_proto::{ErrorBody, ErrorCode};
use coqui_transport::{Channel, Failure};

use crate::config::ShellConfig;
use crate::limit::{Permit, Slots};
use crate::platform::{
    self, ProcessExit, PtySpec, Session, Terminal, find_program, resolve_listed,
};
use crate::policy::{Grant, Permission};

/// What [`serve`] needs besides the stream.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// What this peer may do.
    pub grant: &'a Grant,
    /// The `[shell]` settings.
    pub config: &'a ShellConfig,
    /// The bound on concurrent sessions, shared by every stream.
    pub slots: &'a Slots,
    /// The host's `PATH`, where a shell kind or a program without a directory is looked for.
    pub path: &'a OsStr,
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
/// `unsupported` when shells are disabled; `denied` for a peer without `shell` or an explicit
/// program its grant's program list does not hold; `not_found` for a shell the host lacks, a
/// program or a working directory that does not exist; `busy` at `max_sessions`; `io` for other
/// start failures.
pub fn serve(
    channel: Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<Exit, ErrorBody> {
    let shared = Arc::new(Shared::new(channel, context.config.poll_interval.0));
    let (terminal, _permit) = match start(metadata, context) {
        Ok(started) => started,
        Err(error) => {
            let _ = shared.send(&encode(&Output::Error(error.clone())));
            return Err(error);
        }
    };
    let Terminal {
        session,
        reader,
        writer,
    } = terminal;
    tracing::debug!(pid = session.id(), "shell started");

    if !shared.send(&encode(&Output::Started(Started { pid: session.id() }))) {
        let _ = session.kill();
        session.close_terminal();
        return Ok(Exit {
            killed: true,
            ..Exit::default()
        });
    }

    let chunk = chunk_size(&shared, context.config.chunk_size);
    let spawned = {
        let shared = Arc::clone(&shared);
        let writer = Arc::clone(&writer);
        thread::Builder::new()
            .name("bohio-shell-reader".to_owned())
            .spawn(move || read_output(&shared, reader, &writer, chunk))
    };
    if spawned.is_err() {
        return Err(abandon(&shared, &session));
    }
    let monitor = {
        let shared = Arc::clone(&shared);
        let session = Arc::clone(&session);
        let drain_quiet = context.config.drain_quiet.0;
        let kill_wait = context.config.kill_wait.0;
        thread::Builder::new()
            .name("bohio-shell-monitor".to_owned())
            .spawn(move || monitor(&shared, &session, drain_quiet, kill_wait))
    };
    let Ok(monitor) = monitor else {
        return Err(abandon(&shared, &session));
    };

    receive_frames(&shared, &session, &writer);
    Ok(finish(monitor, &session))
}

/// Whether `grant` still allows the request a `bohio-shell/1` stream opened with: an explicit
/// program is one the program list holds (a shell kind needs only `shell`). A request that cannot be
/// read is covered, so [`serve`] answers it.
pub(crate) fn covers(grant: &Grant, metadata: &[u8]) -> bool {
    Request::from_metadata(metadata).map_or(true, |request| match &request.program {
        Program::Argv { argv } => {
            argv.first()
                .is_none_or(|program| grant.permits_program(program))
                && grant.permits_env(&request.env)
        }
        _ => true,
    })
}

/// Ends a session whose threads could not be started.
fn abandon(shared: &Shared, session: &Session) -> ErrorBody {
    let _ = session.kill();
    session.close_terminal();
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
        loop {
            match self.channel.send_timeout(message, Some(self.poll)) {
                Ok(()) => return true,
                Err(Failure::Timeout | Failure::NoProgress) => {
                    if self.gone.load(Ordering::Acquire) || self.done.load(Ordering::Acquire) {
                        return false;
                    }
                }
                Err(_) => {
                    self.gone.store(true, Ordering::Release);
                    return false;
                }
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

/// The largest output chunk the stream carries: the configured size, less than a message.
fn chunk_size(shared: &Shared, configured: usize) -> usize {
    shared
        .channel
        .capabilities()
        .max_send
        .map_or(configured, |max| configured.min(max.saturating_sub(1)))
        .max(1)
}

fn refuse(code: ErrorCode, message: &'static str) -> ErrorBody {
    ErrorBody::new(code, message)
}

/// Checks the request and starts the program, returning the slot it holds.
fn start<'a>(metadata: &[u8], context: &Context<'a>) -> Result<(Terminal, Permit<'a>), ErrorBody> {
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
    let (program, args) = resolve(&request.program, &request.env, context)?;
    let sized = |n: u16| (1..=MAX_DIMENSION).contains(&n);
    if !sized(request.cols) || !sized(request.rows) {
        return Err(refuse(
            ErrorCode::BadRequest,
            "a terminal is 1 to 32767 columns and rows",
        ));
    }
    let cwd = request.cwd.as_deref().map(PathBuf::from);
    if let Some(cwd) = &cwd
        && !cwd.is_dir()
    {
        return Err(refuse(
            ErrorCode::NotFound,
            "the working directory does not exist",
        ));
    }
    let permit = context.slots.acquire().ok_or_else(|| {
        refuse(
            ErrorCode::Busy,
            "the host is running as many shell sessions as it allows",
        )
    })?;
    let env: Env = request.env;
    let terminal = platform::spawn_terminal(PtySpec {
        program,
        args,
        cwd,
        env,
        cols: request.cols,
        rows: request.rows,
        on_host_death: context.config.host_death,
    })
    .map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => refuse(ErrorCode::NotFound, "the program was not found"),
        std::io::ErrorKind::PermissionDenied => refuse(
            ErrorCode::Denied,
            "the operating system refused to run the program",
        ),
        _ => refuse(ErrorCode::Io, "the shell could not be started"),
    })?;
    Ok((terminal, permit))
}

/// The program and arguments a request names, checked against the grant.
fn resolve(
    program: &Program,
    env: &Env,
    context: &Context<'_>,
) -> Result<(std::ffi::OsString, Vec<std::ffi::OsString>), ErrorBody> {
    match program {
        Program::Kind { kind } => {
            if *kind == ShellKind::Other {
                return Err(refuse(
                    ErrorCode::BadRequest,
                    "this host does not know that kind of shell",
                ));
            }
            let found = find_program(kind.program(), context.path)
                .ok_or_else(|| refuse(ErrorCode::NotFound, "this host has no such shell"))?;
            Ok((found.into_os_string(), Vec::new()))
        }
        Program::Argv { argv } => {
            let Some((first, rest)) = argv.split_first() else {
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
            let named = std::path::Path::new(first);
            let located = if context.grant.restricts_programs() {
                if !context.grant.permits_env(env) {
                    return Err(refuse(
                        ErrorCode::Denied,
                        "under a program list a peer may change only the variables its grant names, and may not clear the environment",
                    ));
                }
                resolve_listed(first, context.path)
            } else if named.components().count() > 1 {
                named.is_file().then(|| named.to_owned())
            } else {
                find_program(first, context.path)
            }
            .ok_or_else(|| refuse(ErrorCode::NotFound, "the program was not found"))?;
            Ok((
                located.into_os_string(),
                rest.iter().map(Into::into).collect(),
            ))
        }
        _ => Err(refuse(
            ErrorCode::Unsupported,
            "this host does not know that way to name a program",
        )),
    }
}

/// Copies the terminal's output to the stream in frames until it ends, then reports so.
fn read_output(
    shared: &Shared,
    mut reader: Box<dyn Read + Send>,
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    chunk: usize,
) {
    let mut buffer = vec![0_u8; chunk];
    #[cfg(windows)]
    let mut queries = platform::CursorQueries::default();
    #[cfg(windows)]
    let asks = answer_queries(writer);
    #[cfg(not(windows))]
    let _ = writer;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                #[cfg(windows)]
                let output = {
                    let (output, asked) = queries.filter(&buffer[..count]);
                    for _ in 0..asked {
                        let _ = asks.send(());
                    }
                    output
                };
                #[cfg(not(windows))]
                let output = buffer[..count].to_vec();
                if output.is_empty() {
                    continue;
                }
                shared.touch();
                if !deliver(shared, output) {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    #[cfg(windows)]
    {
        let held = queries.finish();
        if !held.is_empty() {
            deliver(shared, held);
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

/// Answers the terminal's cursor position queries on a thread of their own, so the reader never
/// waits on a write that holds the writer.
#[cfg(windows)]
fn answer_queries(writer: &Arc<Mutex<Box<dyn Write + Send>>>) -> std::sync::mpsc::Sender<()> {
    let (asks, asked) = std::sync::mpsc::channel::<()>();
    let writer = Arc::clone(writer);
    let _ = thread::Builder::new()
        .name("bohio-shell-answers".to_owned())
        .spawn(move || {
            for () in asked {
                let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = writer
                    .write_all(platform::CURSOR_REPORT)
                    .and_then(|()| writer.flush());
            }
        });
    asks
}

/// Watches the program until it has ended and its output is sent, then sends the last frame.
fn monitor(shared: &Shared, session: &Session, drain_quiet: Duration, kill_wait: Duration) -> Exit {
    let natural = loop {
        if shared.gone.load(Ordering::Acquire) || shared.channel.is_closed() {
            shared.gone.store(true, Ordering::Release);
            shared.killed.store(true, Ordering::Release);
            break false;
        }
        if shared.kill_asked.load(Ordering::Acquire) {
            shared.killed.store(true, Ordering::Release);
            break false;
        }
        match session.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) => thread::sleep(shared.poll),
            Err(_) => break false,
        }
    };

    // The program has exited by itself: its terminal may still deliver the last of its output, so
    // wait until it is quiet (or ended) before ending the rest of the tree.
    if natural {
        while !shared.reader_done.load(Ordering::Acquire)
            && !shared.gone.load(Ordering::Acquire)
            && (shared.sending.load(Ordering::Acquire) || shared.quiet_for() < drain_quiet)
        {
            thread::sleep(shared.poll);
        }
    }
    let ended = session.kill().unwrap_or(ProcessExit {
        code: None,
        signal: None,
    });
    session.close_terminal();

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
        killed: shared.killed.load(Ordering::Acquire),
    };
    shared.send(&encode(&Output::Exit(exit)));
    shared.done.store(true, Ordering::Release);
    exit
}

/// Applies the client's frames until the exchange is over or the stream ends.
fn receive_frames(shared: &Shared, session: &Session, writer: &Arc<Mutex<Box<dyn Write + Send>>>) {
    while !shared.done.load(Ordering::Acquire) {
        match shared.channel.receive(Some(shared.poll)) {
            Ok(Some(frame)) => match Input::decode(&frame) {
                Ok(Input::Data(bytes)) => {
                    let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
                    // A failed write means the program is gone; the monitor sees that.
                    let _ = writer.write_all(&bytes).and_then(|()| writer.flush());
                }
                Ok(Input::Resize(size)) => {
                    let sized = |n: u16| (1..=MAX_DIMENSION).contains(&n);
                    if sized(size.cols) && sized(size.rows) {
                        let _ = session.resize(size.cols, size.rows);
                    }
                }
                Ok(Input::Kill) => shared.kill_asked.store(true, Ordering::Release),
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
fn finish(monitor: JoinHandle<Exit>, session: &Session) -> Exit {
    monitor.join().unwrap_or_else(|_| {
        let ended = session.kill().unwrap_or(ProcessExit {
            code: None,
            signal: None,
        });
        Exit {
            code: ended.code,
            signal: ended.signal,
            killed: true,
        }
    })
}
