//! Operating-system calls: starting a process so its whole tree can be ended, and finding
//! programs on a search path.
//!
//! The tree rules are the ones `coqui_transport::process` documents: a job object on Windows, a
//! process group (and on Linux a walk of `/proc` for processes that left it) on Unix. That module
//! cannot stream a child's standard error, so this one starts the child itself.

#[cfg(unix)]
mod guard;
mod pty;
mod tree;
#[cfg(windows)]
mod winproc;

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use bohio_proto::exec::Env;

use crate::config::HostDeath;

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
        tree::prepare(&mut command, spec.on_host_death)?;
        command.spawn()?
    };
    #[cfg(windows)]
    let winproc::Started {
        mut child,
        stdin,
        stdout,
        stderr,
    } = winproc::start(&spec)?;
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

#[cfg(all(test, windows))]
mod inherit_tests;
