//! One-shot process execution: serves one `bohio-exec/1` stream.
//!
//! [`serve`] runs one request to its end over a channel. It checks the peer's grant first, starts
//! the process in a tree of its own, streams its output as it is written, applies standard input,
//! kill requests and the deadline, and always ends the whole tree: when the stream closes, when the
//! deadline passes and after the process exits.

#![forbid(unsafe_code)]

mod command;

use std::ffi::OsStr;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio_api::guatiao::library::Object;
use bohio_api::model::TerminalExit;
use bohio_api::{ExecSource, Process};
use bohio_proto::compress::Compression;
use bohio_proto::exec::{Exit, Input, Output, Request, Signal, Signalled, Started};
use bohio_proto::{ErrorBody, ErrorCode};
use bohio_transfer::compress::{Caps, Decoder, Encoder, Piece};
use coqui_transport::{Channel, Failure};

use crate::access::{Source, Sources, code_of};
use crate::config::{CompressionConfig, ExecutionConfig};
use crate::fs::compress::{Settings, decode_failure};
use crate::limit::Slots;
use crate::policy::{Grant, Permission};

/// What [`serve`] needs besides the stream.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// What this peer may do.
    pub grant: &'a Grant,
    /// The `[execution]` settings.
    pub config: &'a ExecutionConfig,
    /// The `[compression]` settings, for output and standard input a request asks to compress.
    pub compression: &'a CompressionConfig,
    /// The bound on concurrent processes, shared by every stream.
    pub slots: &'a Slots,
    /// The host's own `PATH`: where a program a grant lists is looked for. The library reads no
    /// environment variable, so the caller says.
    pub path: &'a OsStr,
    /// The node's sources: the one a request names, or the default exec source, starts the process.
    pub sources: &'a Sources,
}

/// A started process and the source it came from.
struct Live {
    process: Object<dyn Process>,
    source: Source<dyn ExecSource>,
}

/// How a process ended, as the wire says it.
#[derive(Debug, Clone, Copy)]
struct ProcessExit {
    code: Option<i32>,
    signal: Option<i32>,
}

impl From<TerminalExit> for ProcessExit {
    fn from(exit: TerminalExit) -> Self {
        Self {
            code: exit.code.and_then(|code| i32::try_from(code).ok()),
            signal: exit.signal.and_then(|signal| i32::try_from(signal).ok()),
        }
    }
}

/// The input of a process as a writer: dropping it ends the input.
struct InputPipe(Arc<Live>);

impl Write for InputPipe {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        match self.0.process.write(data) {
            Ok(taken) if taken > 0 || data.is_empty() => {
                Ok(usize::try_from(taken).unwrap_or(data.len()))
            }
            _ => Err(std::io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for InputPipe {
    fn drop(&mut self) {
        let _ = self.0.process.close_input();
    }
}

/// Serves one `bohio-exec/1` stream whose open carried `metadata`, until the process has ended
/// and its last frame is sent.
///
/// A request that cannot run gets one `error` frame and the same error is returned; the stream is
/// left open for the caller to close. A stream that closes before the process ends ends the
/// process tree; the result is then the process's exit.
///
/// # Errors
///
/// The [`ErrorBody`] sent to the peer: `bad_request` for undecodable or impossible requests,
/// `unsupported` when execution is disabled, `denied` for a peer without `exec` or a program its
/// grant does not list, `busy` at `max_processes`, `not_found` for a program or working directory
/// that does not exist, `io` for other start failures, `unsupported` for a compression the host does
/// not offer, `bad_request` for `auto` as the compression of standard input (the client names the
/// algorithm it sends) and, later, for compressed standard input that expands past `max_expansion`.
///
/// With `output_compression` each flushed chunk of standard output and of standard error is a
/// compressed frame when that is smaller and a raw frame when it is not (which ends that stream's
/// compression context); the two outputs have contexts of their own. `started` says what was used.
pub fn serve(
    channel: Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<Exit, ErrorBody> {
    let poll = context.config.poll_interval.0;
    let shared = Arc::new(Shared {
        channel,
        poll,
        gone: AtomicBool::new(false),
        done: AtomicBool::new(false),
        killed: AtomicBool::new(false),
        sending: AtomicUsize::new(0),
        signalling: AtomicUsize::new(0),
    });
    let (request, live, input, _permit, negotiated) = match start(metadata, context) {
        Ok(started) => started,
        Err(error) => {
            let _ = shared.send(&encode(&Output::Error(error.clone())));
            return Err(error);
        }
    };
    let Negotiated {
        output,
        stdin: stdin_used,
        mut decoder,
    } = negotiated;
    let control = live;
    let mut stdin = input;
    let pid = control
        .process
        .pid()
        .ok()
        .flatten()
        .and_then(|pid| u32::try_from(pid).ok())
        .unwrap_or(0);
    tracing::debug!(pid, "process started");

    let started = Started {
        pid,
        output_compression: output.clone(),
        stdin_compression: stdin_used,
        session: None,
    };
    if !shared.send(&encode(&Output::Started(started))) {
        let _ = control.process.kill();
        return Ok(Exit {
            killed: true,
            ..Exit::default()
        });
    }

    let chunk = chunk_size(&shared, context.config.chunk_size);
    let (done_tx, done_rx) = mpsc::channel();
    for is_stderr in [false, true] {
        let for_pump = Arc::clone(&shared);
        let reader = Arc::clone(&control);
        let done = done_tx.clone();
        let compression = output
            .clone()
            .map(|used| (used, context.compression.caps()));
        let spawned = thread::Builder::new()
            .name("bohio-exec-pump".to_owned())
            .spawn(move || pump(&for_pump, reader, is_stderr, chunk, &done, compression));
        if spawned.is_err() {
            let _ = control.process.kill();
            let error = ErrorBody::new(ErrorCode::Internal, "a thread could not be started");
            let _ = shared.send(&encode(&Output::Error(error.clone())));
            return Err(error);
        }
    }
    drop(done_tx);

    let monitor = {
        let shared = Arc::clone(&shared);
        let control = Arc::clone(&control);
        let timeout = request.timeout_ms.map(Duration::from_millis);
        let drain = context.config.drain_timeout.0;
        thread::Builder::new()
            .name("bohio-exec-monitor".to_owned())
            .spawn(move || monitor(&shared, &control, timeout, drain, &done_rx))
    };
    let Ok(monitor) = monitor else {
        let _ = control.process.kill();
        let error = ErrorBody::new(ErrorCode::Internal, "a thread could not be started");
        let _ = shared.send(&encode(&Output::Error(error.clone())));
        return Err(error);
    };

    receive_frames(&shared, &control, &mut stdin, &mut decoder);
    drop(stdin);
    Ok(finish(monitor, &control))
}

/// Whether `grant` still allows the request a `bohio-exec/1` stream opened with: its program is one
/// the program list holds, or the list allows a script. A request that cannot be read is covered,
/// so [`serve`] answers it.
pub(crate) fn covers(grant: &Grant, metadata: &[u8]) -> bool {
    Request::from_metadata(metadata).map_or(true, |request| command::permitted(&request, grant))
}

/// State the stream's threads share.
struct Shared {
    channel: Arc<dyn Channel>,
    poll: Duration,
    /// The stream ended or failed: nothing more will be read or delivered.
    gone: AtomicBool,
    /// The last frame is sent: a sender waiting for room can stop.
    done: AtomicBool,
    /// The host ended the tree on purpose (kill frame, closed stream or deadline).
    killed: AtomicBool,
    /// Output pumps that hold a chunk the stream has not yet taken: they wait on the reader, not on a process.
    sending: AtomicUsize,
    /// Signal requests being delivered: the exit is not sent before their answers, which the client reads first.
    signalling: AtomicUsize,
}

impl Shared {
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

/// What the request asked of compression, after the host's settings.
struct Negotiated {
    /// What output is sent with, never `auto`.
    output: Option<Compression>,
    /// What standard input is received with.
    stdin: Option<Compression>,
    /// The decoder of standard input, when it is compressed.
    decoder: Option<Decoder>,
}

/// A request that has started: what runs it, its input, the slot it holds and what compression was agreed.
type Begun<'a> = (
    Request,
    Arc<Live>,
    Option<InputPipe>,
    crate::limit::Permit<'a>,
    Negotiated,
);

/// Checks the request and starts the process, returning the slot it holds.
fn start<'a>(metadata: &[u8], context: &Context<'a>) -> Result<Begun<'a>, ErrorBody> {
    let request = Request::from_metadata(metadata)
        .map_err(|_| ErrorBody::new(ErrorCode::BadRequest, "the request cannot be read"))?;
    if !context.config.enabled {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "this host does not run processes",
        ));
    }
    if !context.grant.allows(Permission::Exec) {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            "this peer may not run processes",
        ));
    }
    let source = context.sources.exec(request.source.as_deref())?;
    if !context.sources.permits(context.grant, source.name()) {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            "this peer may not use that exec source",
        ));
    }
    crate::access::refuse_display_forward(context.grant, source.get().describe())?;
    let settings = Settings::new(context.compression);
    let output = settings.resolve(request.output_compression.as_ref())?;
    let stdin = settings.declared(request.stdin_compression.as_ref())?;
    let decoder = stdin
        .as_ref()
        .map(|used| settings.decoder(used))
        .transpose()?;
    let mut spec = command::spec(&request, context.grant, context.path)?;
    spec.on_host_death = Some(context.config.host_death.name().to_owned());
    spec.reset_signals = Some(context.config.reset_signals);
    let stdin_open = spec.stdin;
    let permit = context.slots.acquire().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Busy,
            "the host is running as many processes as it allows",
        )
    })?;
    let process = source.get().start(spec).map_err(|error| {
        let text = error.message().to_string();
        let kind = bohio_api::files::failure_kind(&error);
        let code = match kind {
            Some(bohio_api::files::word::NOT_FOUND) => ErrorCode::NotFound,
            Some(bohio_api::files::word::DENIED) => ErrorCode::Denied,
            Some(bohio_api::files::word::IO) => ErrorCode::Io,
            _ => code_of(error.status),
        };
        let text = kind
            .and_then(|word| text.strip_prefix(word)?.strip_prefix(": "))
            .unwrap_or(&text);
        ErrorBody::new(code, text.to_owned())
    })?;
    let live = Arc::new(Live { process, source });
    let input = stdin_open.then(|| InputPipe(Arc::clone(&live)));
    Ok((
        request,
        live,
        input,
        permit,
        Negotiated {
            output,
            stdin,
            decoder,
        },
    ))
}

/// Copies a pipe to the stream in frames until the pipe ends, then reports so. With `compression`
/// each chunk is one flushed piece of this pipe's own context.
fn pump(
    shared: &Shared,
    live: Arc<Live>,
    is_stderr: bool,
    chunk: usize,
    done: &Sender<()>,
    compression: Option<(Compression, Caps)>,
) {
    let mut encoder = compression.and_then(|(used, caps)| Encoder::new(&used, &caps).ok());
    let mut buffer = vec![0_u8; chunk];
    let stream = if is_stderr {
        bohio_api::exec::STDERR
    } else {
        bohio_api::exec::STDOUT
    };
    let wait = u64::try_from(shared.poll.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    loop {
        match live.process.read(stream, &mut buffer, wait) {
            Ok(0) => {}
            Ok(count) if count < 0 => break,
            Ok(count) => {
                let count = usize::try_from(count).unwrap_or(0);
                let raw = |bytes: Vec<u8>| {
                    if is_stderr {
                        Output::Stderr(bytes)
                    } else {
                        Output::Stdout(bytes)
                    }
                };
                let output = match encoder
                    .as_mut()
                    .map(|encoder| encoder.frame(&buffer[..count]))
                {
                    Some(Ok(Piece::Compressed(piece))) if is_stderr => {
                        Output::StderrCompressed(piece)
                    }
                    Some(Ok(Piece::Compressed(piece))) => Output::StdoutCompressed(piece),
                    _ => raw(buffer[..count].to_vec()),
                };
                shared.sending.fetch_add(1, Ordering::AcqRel);
                let sent = shared.send(&encode(&output));
                shared.sending.fetch_sub(1, Ordering::AcqRel);
                if !sent {
                    // Nobody is listening: keep emptying the pipe so the process is not
                    // held up on a full one while it is being ended.
                    drain(&live, stream, &mut buffer);
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = done.send(());
}

fn drain(live: &Live, stream: u8, buffer: &mut [u8]) {
    while matches!(live.process.read(stream, buffer, 1000), Ok(count) if count >= 0) {}
}

/// Watches the process until it has ended and its output is sent, then sends the last frame.
fn monitor(
    shared: &Shared,
    control: &Live,
    timeout: Option<Duration>,
    drain: Duration,
    pumps: &Receiver<()>,
) -> Exit {
    let started = Instant::now();
    let mut timed_out = false;
    let ended = loop {
        if shared.gone.load(Ordering::Acquire) || shared.channel.is_closed() {
            shared.gone.store(true, Ordering::Release);
            shared.killed.store(true, Ordering::Release);
            break ended(control);
        }
        if !timed_out && timeout.is_some_and(|limit| started.elapsed() >= limit) {
            timed_out = true;
            shared.killed.store(true, Ordering::Release);
            break ended(control);
        }
        if !control.source.is_current() {
            // The source was reconfigured: what runs on it ends.
            shared.killed.store(true, Ordering::Release);
            break ended(control);
        }
        let wait = u64::try_from(shared.poll.as_millis()).unwrap_or(u64::MAX);
        match control.process.wait(wait) {
            Ok(Some(exit)) => break exit.into(),
            Ok(None) => {}
            Err(_) => break ended(control),
        }
    };

    // The process is gone. Its output is complete once both pipes end; a descendant that still
    // holds one open is ended with the rest of the tree after `drain`.
    let mut open = 2_u8;
    if !wait_for(pumps, &mut open, drain, &shared.sending) {
        let _ = control.process.kill();
        wait_for(pumps, &mut open, drain, &shared.sending);
    }
    let _ = control.process.kill();

    // A signal that ended the process is answered before its exit.
    let waited = Instant::now();
    while shared.signalling.load(Ordering::Acquire) > 0
        && !shared.gone.load(Ordering::Acquire)
        && waited.elapsed() < drain
    {
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

/// Ends the tree and returns how the process ended.
fn ended(control: &Live) -> ProcessExit {
    control.process.kill().map_or(
        ProcessExit {
            code: None,
            signal: None,
        },
        Into::into,
    )
}

/// Waits until `open` more pumps have reported or `limit` passes; whether they all reported. A pump
/// that is handing output to a reader still being slow is waiting for the reader, not for a process
/// that holds its pipe, so `limit` starts again for as long as one is.
fn wait_for(pumps: &Receiver<()>, open: &mut u8, limit: Duration, sending: &AtomicUsize) -> bool {
    let mut deadline = Instant::now() + limit;
    while *open > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        match pumps.recv_timeout(left) {
            Ok(()) => *open -= 1,
            // Every pump reporting drops the last sender: nothing more will come.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                *open = 0;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if sending.load(Ordering::Acquire) == 0 {
                    return false;
                }
                deadline = Instant::now() + limit;
            }
        }
    }
    true
}

/// Applies the client's frames until the exchange is over or the stream ends.
fn receive_frames(
    shared: &Shared,
    control: &Live,
    stdin: &mut Option<InputPipe>,
    decoder: &mut Option<Decoder>,
) {
    while !shared.done.load(Ordering::Acquire) {
        match shared.channel.receive(Some(shared.poll)) {
            Ok(Some(frame)) => match Input::decode(&frame) {
                Ok(Input::StdinCompressed(piece)) => {
                    let Some(decoder) = decoder.as_mut() else {
                        refuse(
                            shared,
                            control,
                            ErrorBody::new(
                                ErrorCode::BadRequest,
                                "compressed input needs a stdin_compression in the request",
                            ),
                        );
                        continue;
                    };
                    if let Some(pipe) = stdin.as_mut() {
                        match decoder.feed(&piece, pipe) {
                            Ok(_) => {
                                if pipe.flush().is_err() {
                                    *stdin = None;
                                }
                            }
                            Err(bohio_transfer::compress::DecodeError::Write(_)) => *stdin = None,
                            Err(error) => refuse(shared, control, decode_failure(&error)),
                        }
                    }
                }
                Ok(Input::Stdin(bytes)) => {
                    // A raw frame ends the compression context.
                    if let Some(decoder) = decoder.as_mut() {
                        decoder.reset();
                    }
                    if let Some(pipe) = stdin.as_mut()
                        && pipe.write_all(&bytes).and_then(|()| pipe.flush()).is_err()
                    {
                        *stdin = None;
                    }
                }
                Ok(Input::StdinEnd) => *stdin = None,
                Ok(Input::Kill) => {
                    if matches!(control.process.wait(0), Ok(None)) {
                        shared.killed.store(true, Ordering::Release);
                    }
                    let _ = control.process.kill();
                }
                Ok(Input::Signal(signal)) => deliver(shared, control, signal),
                // A frame this version does not know, or a body that is not what its tag says,
                // is skipped: the exchange goes on.
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

/// Delivers `signal` to the process and tells the client whether it arrived.
fn deliver(shared: &Shared, control: &Live, signal: Signal) {
    shared.signalling.fetch_add(1, Ordering::AcqRel);
    let delivery = control
        .process
        .signal(signal.name())
        .unwrap_or(bohio_api::Delivery {
            delivered: false,
            ended: false,
            message: Some("the source could not deliver it".to_owned()),
        });
    if delivery.ended {
        shared.killed.store(true, Ordering::Release);
    }
    let _ = shared.send(&encode(&Output::Signalled(Signalled {
        signal,
        delivered: delivery.delivered,
        message: delivery.message,
    })));
    shared.signalling.fetch_sub(1, Ordering::AcqRel);
}

/// Ends the process and tells the client why: its input could not be taken.
fn refuse(shared: &Shared, control: &Live, error: ErrorBody) {
    shared.killed.store(true, Ordering::Release);
    let _ = shared.send(&encode(&Output::Error(error)));
    let _ = control.process.kill();
}

/// Waits for the monitor, which always ends once the process is gone.
fn finish(monitor: JoinHandle<Exit>, control: &Live) -> Exit {
    monitor.join().unwrap_or_else(|_| {
        let ended = ended(control);
        Exit {
            code: ended.code,
            signal: ended.signal,
            timed_out: false,
            killed: true,
        }
    })
}
