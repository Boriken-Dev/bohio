//! The `native` provider's processes: programs started on this node's own operating system, each in a tree that is
//! always ended.
//!
//! The host decides the grant before a source sees a request; this source finds the program and the shell, starts it
//! in its own tree and serves its two outputs through bounded queues, so a peer that reads slowly holds the program
//! back instead of the node's memory.

#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use bohio_api::exec::{STDERR, STDOUT};
use bohio_api::files::{failure, word};
use bohio_api::guatiao::Status;
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::model::{Delivery, ExecStart, SourceInfo, TerminalExit};
use bohio_api::{ExecSource, Process, unavailable, vocab};

use super::{ID, Native, env_of, exit_of};
use crate::config::HostDeath;
use crate::platform::{self, Control, ProcessExit, Signal, Spec, resolve_listed};

/// How long the wait for a program's exit sleeps between looks at it when no setting names one: the wait is a
/// poll of an exit that has no event to wait on.
const WAIT_SLICE: Duration = Duration::from_millis(2);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn refuse(status: Status, message: &str) -> ProviderError {
    ProviderError::new(status, message)
}

impl ExecSource for Native {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        let signals = platform::exec_signals().len() == Signal::ALL.len();
        let mut offers = vec![
            vocab::exec::START,
            vocab::exec::STDIN,
            vocab::exec::STDERR,
            vocab::exec::SHELL,
            vocab::exec::CWD,
            vocab::exec::ENV,
            vocab::exec::LISTED,
            vocab::exec::KILL_TREE,
            vocab::exec::EXIT_STATUS,
        ];
        let mut lacks = Vec::new();
        // Without every signal the source still delivers the ones this system has, which `info.exec` lists.
        offers.push(vocab::exec::SIGNALS);
        if !signals {
            lacks.push("signal_names");
        }
        Ok(SourceInfo::new(ID, &offers, &lacks))
    }

    fn start(&self, request: ExecStart) -> Result<Object<dyn Process>, ProviderError> {
        let path = request
            .path
            .as_deref()
            .map(OsString::from)
            .unwrap_or_default();
        let (program, args, raw_args) = line_of(&request, &path)?;
        if let Some(cwd) = request.cwd.as_deref()
            && !std::path::Path::new(cwd).is_dir()
        {
            return Err(failure(
                word::NOT_FOUND,
                "the working directory does not exist",
            ));
        }
        let spec = Spec {
            program,
            args,
            raw_args,
            cwd: request.cwd.as_deref().map(PathBuf::from),
            env: env_of(request.clear_env, &request.env),
            stdin: request.stdin,
            on_host_death: match request.on_host_death.as_deref() {
                Some("term") => HostDeath::Term,
                Some("leave") => HostDeath::Leave,
                _ => HostDeath::Kill,
            },
            reset_signals: request.reset_signals.unwrap_or(true),
        };
        let spawned = platform::spawn(spec).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => failure(word::NOT_FOUND, "the program was not found"),
            std::io::ErrorKind::PermissionDenied => failure(
                word::DENIED,
                "the operating system refused to run the program",
            ),
            _ => failure(word::IO, "the process could not be started"),
        })?;
        let chunk = usize::try_from(self.settings.read_chunk).unwrap_or(usize::MAX);
        let queued = usize::try_from(self.settings.queue_chunks).unwrap_or(usize::MAX);
        let platform::Spawned {
            control,
            stdin,
            stdout,
            stderr,
        } = spawned;
        Ok(Running {
            control,
            input: Mutex::new(stdin),
            out: Pipe::start(stdout, chunk, queued),
            err: Pipe::start(stderr, chunk, queued),
        }
        .into_object())
    }
}

type Line = (OsString, Vec<OsString>, Option<OsString>);

/// The program and arguments a request runs: its own, found as the grant says, or a shell running its script.
fn line_of(request: &ExecStart, path: &OsStr) -> Result<Line, ProviderError> {
    if let Some(kind) = request.shell.as_deref() {
        let script = request.script.as_deref().unwrap_or_default();
        return shell_line(kind, script);
    }
    let Some((program, args)) = request.argv.split_first() else {
        return Err(refuse(
            Status::GUATIAO_ERR_BAD_VALUE,
            "the argument vector is empty",
        ));
    };
    let program = if request.listed {
        resolve_listed(program, path)
            .ok_or_else(|| failure(word::NOT_FOUND, "the program was not found"))?
            .into_os_string()
    } else {
        OsString::from(program)
    };
    Ok((program, args.iter().map(OsString::from).collect(), None))
}

/// The program and arguments that make the shell `kind` run `script`.
fn shell_line(kind: &str, script: &str) -> Result<Line, ProviderError> {
    Ok(match kind {
        "sh" | "bash" | "zsh" => (OsString::from(kind), vec!["-c".into(), script.into()], None),
        "powershell" | "pwsh" => (
            OsString::from(kind),
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
            None,
        ),
        // `cmd` parses its own command line: the script goes through as written.
        "cmd" if cfg!(windows) => (
            OsString::from("cmd"),
            vec!["/C".into()],
            Some(OsString::from(format!(" {script}"))),
        ),
        "cmd" => (
            OsString::from("cmd"),
            vec!["/C".into(), script.into()],
            None,
        ),
        _ => {
            return Err(refuse(
                Status::GUATIAO_ERR_BAD_VALUE,
                "that shell kind is not known to this host",
            ));
        }
    })
}

/// What a reader thread holds back: the chunks it has read and not yet been asked for.
struct Pipe {
    queue: Mutex<Queue>,
    /// The write end of a pipe the reader thread also waits on: it closes when the process is let go, which wakes a thread
    /// a surviving descendant would otherwise keep blocked on the output pipe for good.
    #[cfg(unix)]
    _wake: Option<std::os::fd::OwnedFd>,
}

struct Queue {
    chunks: Receiver<Vec<u8>>,
    /// The part of a chunk a short `read` did not take.
    left: Vec<u8>,
    /// The offset into `left`.
    at: usize,
}

/// What the reader thread waits on besides its pipe, where the system lets it.
#[cfg(unix)]
trait Waitable: std::os::fd::AsFd {}
#[cfg(unix)]
impl<T: std::os::fd::AsFd> Waitable for T {}
#[cfg(not(unix))]
trait Waitable {}
#[cfg(not(unix))]
impl<T> Waitable for T {}

/// Waits until `reader` has something to read (or ended), or until `wake` is closed; `false` when it was.
#[cfg(unix)]
fn readable(reader: &impl std::os::fd::AsFd, wake: &std::os::fd::OwnedFd) -> bool {
    use rustix::event::{PollFd, PollFlags, poll};
    loop {
        let mut fds = [
            PollFd::new(reader, PollFlags::IN),
            PollFd::new(wake, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Ok(_) => return fds[1].revents().is_empty(),
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return true,
        }
    }
}

impl Pipe {
    fn start<R: Read + Send + Waitable + 'static>(
        mut reader: R,
        chunk: usize,
        queued: usize,
    ) -> Self {
        let (sender, chunks): (SyncSender<Vec<u8>>, _) = sync_channel(queued.max(1));
        let size = chunk.max(1);
        #[cfg(unix)]
        let (wake_read, wake_write) =
            match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
                Ok((read, write)) => (Some(read), Some(write)),
                Err(_) => (None, None),
            };
        // The thread ends when the pipe does (the tree is ended), when the process is let go, or when nobody holds the
        // queue any more.
        let _ = thread::Builder::new()
            .name("bohio-exec-read".to_owned())
            .spawn(move || {
                let mut buffer = vec![0_u8; size];
                loop {
                    #[cfg(unix)]
                    if let Some(wake) = &wake_read
                        && !readable(&reader, wake)
                    {
                        return;
                    }
                    match reader.read(&mut buffer) {
                        Ok(0) => return,
                        Ok(count) => {
                            if sender.send(buffer[..count].to_vec()).is_err() {
                                return;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => return,
                    }
                }
            });
        Self {
            queue: Mutex::new(Queue {
                chunks,
                left: Vec::new(),
                at: 0,
            }),
            #[cfg(unix)]
            _wake: wake_write,
        }
    }

    /// Bytes into `dst`, waiting up to `wait` for some; `0` when none came, `-1` when the pipe has ended.
    fn read(&self, dst: &mut [u8], wait: Duration) -> i64 {
        let mut queue = lock(&self.queue);
        if queue.at >= queue.left.len() {
            let next = if wait.is_zero() {
                queue.chunks.try_recv().map_err(|error| match error {
                    std::sync::mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                    std::sync::mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                })
            } else {
                queue.chunks.recv_timeout(wait)
            };
            match next {
                Ok(chunk) => {
                    queue.left = chunk;
                    queue.at = 0;
                }
                Err(RecvTimeoutError::Timeout) => return 0,
                Err(RecvTimeoutError::Disconnected) => return -1,
            }
        }
        let take = (queue.left.len() - queue.at).min(dst.len());
        let from = queue.at;
        dst[..take].copy_from_slice(&queue.left[from..from + take]);
        queue.at += take;
        i64::try_from(take).unwrap_or(i64::MAX)
    }
}

/// One started program: its tree, its input and its two outputs.
struct Running {
    control: Arc<Control>,
    input: Mutex<Option<platform::Stdin>>,
    out: Pipe,
    err: Pipe,
}

impl Process for Running {
    fn read(&self, stream: u8, dst: &mut [u8], wait_ms: u64) -> Result<i64, ProviderError> {
        let pipe = if stream == STDERR {
            &self.err
        } else {
            &self.out
        };
        debug_assert!(stream == STDOUT || stream == STDERR);
        Ok(pipe.read(dst, Duration::from_millis(wait_ms)))
    }

    fn write(&self, data: &[u8]) -> Result<u64, ProviderError> {
        let mut input = lock(&self.input);
        let Some(pipe) = input.as_mut() else {
            return Err(unavailable("standard input is closed"));
        };
        if pipe.write_all(data).and_then(|()| pipe.flush()).is_err() {
            *input = None;
            return Err(unavailable("standard input is closed"));
        }
        Ok(data.len() as u64)
    }

    fn close_input(&self) -> Result<(), ProviderError> {
        *lock(&self.input) = None;
        Ok(())
    }

    fn wait(&self, wait_ms: u64) -> Result<Option<TerminalExit>, ProviderError> {
        let until = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            match self.control.try_wait() {
                Ok(Some(exit)) => return Ok(Some(exit_of(exit))),
                Ok(None) => {}
                Err(_) => return Err(unavailable("the program cannot be looked at")),
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            thread::sleep(left.min(WAIT_SLICE));
        }
    }

    fn kill(&self) -> Result<TerminalExit, ProviderError> {
        Ok(exit_of(self.control.kill().unwrap_or(ProcessExit {
            code: None,
            signal: None,
        })))
    }

    fn pid(&self) -> Result<Option<u64>, ProviderError> {
        Ok(Some(u64::from(self.control.id())))
    }

    fn signal(&self, name: &str) -> Result<Delivery, ProviderError> {
        let signal = Signal::ALL
            .into_iter()
            .find(|signal| signal.name() == name)
            .ok_or_else(|| refuse(Status::GUATIAO_ERR_BAD_VALUE, "not a signal"))?;
        let running = matches!(self.control.try_wait(), Ok(None));
        let delivery = self.control.signal(signal);
        Ok(Delivery {
            delivered: delivery.delivered,
            ended: delivery.ended && running,
            message: delivery.message.map(str::to_owned),
        })
    }
}
