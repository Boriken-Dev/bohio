//! Operating-system calls: starting a process so its whole tree can be ended, and finding
//! programs on a search path.
//!
//! The tree rules are the ones `coqui_transport::process` documents: a job object on Windows, a
//! process group (and on Linux a walk of `/proc` for processes that left it) on Unix. That module
//! cannot stream a child's standard error, so this one starts the child itself.

#[cfg(unix)]
mod guard;
pub(crate) mod gui;
mod pty;
#[cfg(target_os = "linux")]
mod reap;
mod tree;
#[cfg(windows)]
pub(crate) mod winacl;
#[cfg(windows)]
pub(crate) mod winfs;
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the byte layouts are `winfs`'s and are tested on every platform"
    )
)]
mod winfs_layout;
#[cfg(windows)]
mod winproc;

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bohio_proto::exec::Env;

use crate::knobs::HostDeath;

use self::tree::{Poll, Tree};
#[cfg(windows)]
use self::winproc::Child;

/// The parent's end of a started process's standard input.
#[cfg(unix)]
pub type Stdin = ChildStdin;
/// The parent's end of a started process's standard output.
#[cfg(unix)]
pub type Stdout = ChildStdout;
/// The parent's end of a started process's standard error.
#[cfg(unix)]
pub type Stderr = ChildStderr;
/// The parent's end of a started process's standard input.
#[cfg(windows)]
pub type Stdin = std::io::PipeWriter;
/// The parent's end of a started process's standard output.
#[cfg(windows)]
pub type Stdout = std::io::PipeReader;
/// The parent's end of a started process's standard error.
#[cfg(windows)]
pub type Stderr = std::io::PipeReader;

#[cfg(windows)]
pub use self::pty::{CURSOR_REPORT, CursorQueries};
pub use self::pty::{PtySpec, Session, Terminal, spawn as spawn_terminal};

pub use bohio_proto::signal::Signal;

/// What delivering a signal came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The signal reached the program.
    pub delivered: bool,
    /// The host ended the tree to deliver it (`kill`, and `terminate` where there is no signal).
    pub ended: bool,
    /// Why it was not delivered, or what delivering it meant here.
    pub message: Option<&'static str>,
}

impl Delivery {
    /// Delivered as asked.
    pub const fn done() -> Self {
        Self {
            delivered: true,
            ended: false,
            message: None,
        }
    }

    /// Not delivered, for `reason`.
    pub const fn refused(reason: &'static str) -> Self {
        Self {
            delivered: false,
            ended: false,
            message: Some(reason),
        }
    }

    /// The tree was ended to deliver the signal.
    pub const fn tree_ended(note: Option<&'static str>) -> Self {
        Self {
            delivered: true,
            ended: true,
            message: note,
        }
    }
}

/// The signals this host delivers to a program started without a terminal.
pub fn exec_signals() -> Vec<Signal> {
    if cfg!(unix) {
        Signal::ALL.to_vec()
    } else {
        // `interrupt` needs a console the host shares; `info` lists it only when this one has one.
        let mut signals = vec![Signal::Terminate, Signal::Kill];
        if cfg!(windows) && has_console() {
            signals.insert(0, Signal::Interrupt);
        }
        signals
    }
}

/// The signals this host delivers to a program on a terminal.
pub fn terminal_signals() -> Vec<Signal> {
    if cfg!(unix) {
        Signal::ALL.to_vec()
    } else {
        vec![Signal::Interrupt, Signal::Terminate, Signal::Kill]
    }
}

/// Whether this host has a console its programs can share (Windows; always `false` elsewhere).
pub fn has_console() -> bool {
    #[cfg(windows)]
    {
        winproc::host_has_console()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// How a process ended, as the operating system reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessExit {
    /// The exit code, when it ended by itself.
    pub code: Option<i32>,
    /// The signal that ended it, on Unix.
    pub signal: Option<i32>,
}

/// What to start.
#[derive(Debug)]
pub struct Spec {
    /// The program; looked up on `PATH` when it names no directory. Never run through a shell.
    pub program: OsString,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// An argument line handed to the process as written, for `cmd /C` on Windows.
    pub raw_args: Option<OsString>,
    /// The working directory; the caller's when absent.
    pub cwd: Option<PathBuf>,
    /// Changes to the environment.
    pub env: Env,
    /// Whether standard input is a pipe; otherwise it is empty.
    pub stdin: bool,
    /// What happens to the process's group if this process dies without ending it (Unix).
    pub on_host_death: HostDeath,
    /// Start the process with SIGINT, SIGQUIT and SIGHUP at their default action, not what this process inherited (Unix).
    pub reset_signals: bool,
}

/// A started process and its pipes.
#[derive(Debug)]
pub struct Spawned {
    /// Ends and observes the process; ends the whole tree when the last handle drops.
    pub control: Arc<Control>,
    /// Standard input, when asked for.
    pub stdin: Option<Stdin>,
    /// Standard output.
    pub stdout: Stdout,
    /// Standard error.
    pub stderr: Stderr,
}

/// Starts the thread that collects, every `interval`, the ended guardians this process adopted: where the host is
/// process 1 (a container's entrypoint) each ended program leaves its guardian as a zombie otherwise. Does nothing
/// off Linux. It collects only guardians, never a process the host started and waits for.
pub fn collect_adopted_guardians(interval: Duration) {
    #[cfg(target_os = "linux")]
    reap::start(interval);
    #[cfg(not(target_os = "linux"))]
    let _ = interval;
}

/// Starts `spec` in a tree of its own.
///
/// On Windows the child inherits its three standard streams and no other handle of this process.
pub fn spawn(spec: Spec) -> io::Result<Spawned> {
    #[cfg(unix)]
    let mut child = {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        let _ = &spec.raw_args;
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        if spec.env.clear {
            command.env_clear();
        }
        for (name, value) in &spec.env.set {
            command.env(name, value);
        }
        for name in &spec.env.remove {
            command.env_remove(name);
        }
        command
            .stdin(if spec.stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if spec.reset_signals {
            guard::reset_signals(&mut command);
        }
        tree::prepare(&mut command, spec.on_host_death)?;
        command.spawn()?
    };
    #[cfg(windows)]
    let (
        tree,
        winproc::Started {
            child,
            stdin,
            stdout,
            stderr,
        },
    ) = {
        // The process starts inside the job, so nothing it starts at once is outside it.
        let job = tree::job()?;
        let _ = (&spec.on_host_death, spec.reset_signals);
        let started = winproc::start(&spec, &job)?;
        (Tree::holding(job), started)
    };
    #[cfg(unix)]
    let tree = match tree::contain(&child) {
        Ok(tree) => tree,
        Err(error) => {
            // Nothing can end this tree later, so end the child now.
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    #[cfg(unix)]
    let (stdin, stdout, stderr) = {
        let stdin = child.stdin.take();
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("the child's pipes are missing"));
        };
        (stdin, stdout, stderr)
    };
    Ok(Spawned {
        control: Arc::new(Control {
            inner: Mutex::new(Inner {
                child,
                tree,
                exit: None,
                collected: false,
            }),
        }),
        stdin,
        stdout,
        stderr,
    })
}

/// Observes and ends one process and its tree.
#[derive(Debug)]
pub struct Control {
    inner: Mutex<Inner>,
}

struct Inner {
    child: Child,
    tree: Tree,
    /// Set once the process is known to have exited.
    exit: Option<ProcessExit>,
    /// Whether the child has been waited for, after which its id may be another process's.
    collected: bool,
}

impl core::fmt::Debug for Inner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Inner")
            .field("pid", &self.child.id())
            .field("exit", &self.exit)
            .field("collected", &self.collected)
            .finish_non_exhaustive()
    }
}

impl Control {
    /// The process id.
    pub fn id(&self) -> u32 {
        self.lock().child.id()
    }

    /// How the process ended, or `None` while it runs. On Unix an ended process stays uncollected
    /// until [`Control::kill`] so its group can still be signalled.
    pub fn try_wait(&self) -> io::Result<Option<ProcessExit>> {
        let mut inner = self.lock();
        if let Some(exit) = inner.exit {
            return Ok(Some(exit));
        }
        match tree::poll(&mut inner.child)? {
            Poll::Running => Ok(None),
            #[cfg(unix)]
            Poll::Exited(exit) => {
                inner.exit = Some(exit);
                Ok(Some(exit))
            }
            #[cfg(windows)]
            Poll::Collected(exit) => {
                inner.exit = Some(exit);
                inner.collected = true;
                Ok(Some(exit))
            }
        }
    }

    /// Delivers `signal` to the program. `kill` ends the whole tree; on Unix any other signal goes to the
    /// program's process group; on Windows `terminate` ends the tree (there is no `SIGTERM`), `interrupt`
    /// is a break event when the host has a console and every other signal is not delivered.
    pub fn signal(&self, signal: Signal) -> Delivery {
        if signal == Signal::Kill {
            return match self.kill() {
                Ok(_) => Delivery::tree_ended(None),
                Err(_) => Delivery::refused("the process could not be ended"),
            };
        }
        #[cfg(unix)]
        {
            let inner = self.lock();
            if inner.collected {
                return Delivery::refused("the process has ended");
            }
            if tree::send_group(inner.child.id(), signal) {
                Delivery::done()
            } else {
                Delivery::refused("no process is left to receive it")
            }
        }
        #[cfg(windows)]
        {
            match signal {
                Signal::Terminate => match self.kill() {
                    Ok(_) => Delivery::tree_ended(Some(
                        "Windows has no SIGTERM: the process tree was ended",
                    )),
                    Err(_) => Delivery::refused("the process could not be ended"),
                },
                Signal::Interrupt => {
                    let inner = self.lock();
                    if inner.collected {
                        return Delivery::refused("the process has ended");
                    }
                    match inner.child.interrupt() {
                        Ok(()) => Delivery::done(),
                        Err(_) => {
                            Delivery::refused("this host has no console to deliver a break through")
                        }
                    }
                }
                _ => Delivery::refused("Windows has no such signal"),
            }
        }
    }

    /// Ends the whole tree, collects the process and returns how it ended. Repeatable.
    pub fn kill(&self) -> io::Result<ProcessExit> {
        let mut inner = self.lock();
        let inner = &mut *inner;
        if !inner.collected {
            tree::end(&inner.tree, &inner.child);
            let status = inner.child.wait()?;
            inner.collected = true;
            if inner.exit.is_none() {
                inner.exit = Some(tree::exit_of(status));
            }
        } else {
            // Collected already: the process is gone, and on Windows closing the job still ends
            // what it left behind.
            #[cfg(windows)]
            tree::end(&inner.tree, &inner.child);
        }
        inner
            .exit
            .ok_or_else(|| io::Error::other("the process has no exit status"))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// Looks for `program` in the directories of `path`, as a shell would: a name with a directory
/// is taken as it is. On Windows `.exe` is tried when the name has no extension.
pub fn find_program(program: &str, path: &OsStr) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    let names: Vec<String> = if cfg!(windows) && Path::new(program).extension().is_none() {
        vec![format!("{program}.exe"), program.to_owned()]
    } else {
        vec![program.to_owned()]
    };
    std::env::split_paths(path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// The absolute path a program a grant lists names, looked up in the directories of `path`
/// (the service's own search path): an absolute name is taken as it is, a bare name is searched
/// for, and any other name is refused, since a relative path would resolve against a directory
/// the peer can choose. `None` when there is no such file.
pub fn resolve_listed(program: &str, path: &OsStr) -> Option<PathBuf> {
    let named = Path::new(program);
    let found = if named.is_absolute() {
        named.is_file().then(|| named.to_owned())
    } else if named.components().count() == 1 {
        find_program(program, path)
    } else {
        None
    }?;
    std::path::absolute(found).ok()
}

/// Whether `path` names a batch file as Windows reads it: by its extension without case, after the trailing dots and
/// spaces Windows drops from a name (`x.bat.` is `x.bat`).
#[must_use]
pub fn is_batch(path: &Path) -> bool {
    let name = path.file_name().map(|name| name.to_string_lossy());
    name.as_deref()
        .map(|name| name.trim_end_matches(['.', ' ']))
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(_, extension)| {
            extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
        })
}

#[cfg(test)]
mod batch_tests {
    use std::path::Path;

    use super::is_batch;

    #[test]
    fn a_batch_file_is_known_by_the_name_windows_reads() {
        for name in [
            "a.bat",
            "a.CMD",
            "dir/a.bat",
            "a.bat.",
            "a.cmd. .",
            "a.Bat ",
        ] {
            assert!(is_batch(Path::new(name)), "{name}");
        }
        for name in ["a.exe", "bat", "a.batx", "a.bat.exe", "", "a."] {
            assert!(!is_batch(Path::new(name)), "{name}");
        }
    }
}

#[cfg(all(test, windows))]
mod inherit_tests;

/// Held by a unit test while it holds connections open (so no test starts a process meanwhile) and by one that starts
/// a child with the standard library, which inherits every inheritable handle of this process.
#[cfg(all(test, windows))]
pub(crate) static SPAWN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(all(test, target_os = "linux"))]
mod signal_tests;
