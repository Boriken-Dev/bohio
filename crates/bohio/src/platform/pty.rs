//! A program on a pseudo-terminal, through `portable-pty`: `openpty` on Unix, ConPTY on Windows.
//!
//! The program's tree is ended the way [`super::spawn`]'s is: a job object on Windows, a process
//! group on Unix. `portable-pty` starts the child itself (on Unix as a session and group leader),
//! so on Windows the child joins its job just after it has started: a process it starts in that
//! instant is outside the tree.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::config::HostDeath;
use bohio_proto::exec::Env;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

#[cfg(unix)]
use super::guard;
use super::tree::{self, Tree};
use super::{Delivery, ProcessExit, Signal};

/// `TERM` for a program whose environment does not set it.
pub const DEFAULT_TERM: &str = "xterm-256color";

/// What to start on the terminal.
#[derive(Debug)]
pub struct PtySpec {
    /// The program; looked up on `PATH` when it names no directory.
    pub program: OsString,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// The working directory; the caller's when absent.
    pub cwd: Option<PathBuf>,
    /// Changes to the environment.
    pub env: Env,
    /// Terminal width in columns.
    pub cols: u16,
    /// Terminal height in rows.
    pub rows: u16,
    /// What happens to the program's tree if this process dies without ending it (Unix).
    #[cfg_attr(
        windows,
        allow(
            dead_code,
            reason = "a job object ends a Windows tree whenever its owner is gone"
        )
    )]
    pub on_host_death: HostDeath,
}

/// A program on its terminal.
pub struct Terminal {
    /// Observes and ends the program and its tree.
    pub session: Arc<Session>,
    /// What the terminal produces; ends when the program and everything holding the terminal are
    /// gone, or on Windows when the terminal is closed.
    pub reader: Box<dyn Read + Send>,
    /// What is typed at the terminal.
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

/// Observes and ends one program on a terminal and its tree.
pub struct Session {
    pid: u32,
    #[cfg_attr(
        not(windows),
        allow(dead_code, reason = "a process group needs no object")
    )]
    tree: Tree,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    state: Mutex<State>,
    /// The guardian that ends the tree if this process dies, until the program is collected.
    #[cfg(unix)]
    watch: Mutex<Option<guard::Watch>>,
}

struct State {
    /// Set once the program is known to have exited.
    exit: Option<ProcessExit>,
    /// Whether the child has been waited for, after which its id may be another process's.
    collected: bool,
}

impl core::fmt::Debug for Session {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Session")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An error from `portable-pty`, keeping the kind of the operating system's error it holds.
fn io_error(error: &(dyn std::error::Error + 'static)) -> io::Error {
    let mut kind = io::ErrorKind::Other;
    let mut cause = Some(error);
    while let Some(current) = cause {
        if let Some(found) = current.downcast_ref::<io::Error>() {
            kind = found.kind();
            break;
        }
        cause = current.source();
    }
    io::Error::new(kind, error.to_string())
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Starts `spec` on a new terminal, in a tree of its own.
pub fn spawn(spec: PtySpec) -> io::Result<Terminal> {
    let pair = native_pty_system()
        .openpty(size(spec.cols, spec.rows))
        .map_err(|error| io_error(&*error))?;
    let mut command = CommandBuilder::new(&spec.program);
    command.args(&spec.args);
    if spec.env.clear {
        command.env_clear();
    }
    for (name, value) in &spec.env.set {
        command.env(name, value);
    }
    for name in &spec.env.remove {
        command.env_remove(name);
    }
    if command.get_env("TERM").is_none() {
        command.env("TERM", DEFAULT_TERM);
    }
    if let Some(cwd) = &spec.cwd {
        command.cwd(cwd);
    }
    #[cfg_attr(
        not(windows),
        allow(
            unused_mut,
            reason = "only Windows ends a child that cannot be contained"
        )
    )]
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| io_error(&*error))?;
    let pid = child
        .process_id()
        .ok_or_else(|| io::Error::other("the terminal's process has no id"))?;

    #[cfg(unix)]
    let tree = Tree;
    #[cfg(unix)]
    let watch = match guard::watch_session(pid, spec.on_host_death) {
        Ok(watch) => watch,
        Err(error) => {
            // Nothing would end this tree if the host died, so end the child now.
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    #[cfg(windows)]
    let tree = {
        let contained = child
            .as_raw_handle()
            .ok_or_else(|| io::Error::other("the terminal's process has no handle"))
            .and_then(|handle| tree::contain_handle(handle as isize));
        match contained {
            Ok(tree) => tree,
            Err(error) => {
                // Nothing can end this tree later, so end the child now.
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    };

    // The child owns the slave now; holding it here would keep the terminal open past its exit.
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| io_error(&*error))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| io_error(&*error))?;
    Ok(Terminal {
        session: Arc::new(Session {
            pid,
            tree,
            child: Mutex::new(child),
            master: Mutex::new(Some(pair.master)),
            state: Mutex::new(State {
                exit: None,
                collected: false,
            }),
            #[cfg(unix)]
            watch: Mutex::new(watch),
        }),
        reader,
        writer: Arc::new(Mutex::new(writer)),
    })
}

fn exit_of(status: &portable_pty::ExitStatus) -> ProcessExit {
    match status.signal() {
        Some(name) => ProcessExit {
            code: None,
            signal: Some(signal_number(name)),
        },
        // `portable-pty` keeps Windows' unsigned exit code; it is reported as the signed 32-bit
        // value the standard library gives (`0xFFFFFFFF` is -1).
        None => ProcessExit {
            code: Some(status.exit_code().cast_signed()),
            signal: None,
        },
    }
}

/// `portable-pty` reports a signal by name.
fn signal_number(name: &str) -> i32 {
    match name.trim_start_matches("SIG").to_ascii_uppercase().as_str() {
        "HUP" | "HANGUP" => 1,
        "INT" | "INTERRUPT" => 2,
        "QUIT" => 3,
        "ABRT" | "ABORTED" => 6,
        "KILL" | "KILLED" => 9,
        "SEGV" | "SEGMENTATION FAULT" => 11,
        "PIPE" | "BROKEN PIPE" => 13,
        "TERM" | "TERMINATED" => 15,
        _ => -1,
    }
}

impl Session {
    /// The shell's process id.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// How the program ended, or `None` while it runs. On Unix an ended program stays uncollected
    /// until [`Session::kill`] so its group can still be signalled.
    pub fn try_wait(&self) -> io::Result<Option<ProcessExit>> {
        let mut state = lock(&self.state);
        if let Some(exit) = state.exit {
            return Ok(Some(exit));
        }
        #[cfg(unix)]
        {
            let exit = tree::poll_id(self.pid)?;
            state.exit = exit;
            Ok(exit)
        }
        #[cfg(windows)]
        {
            let status = lock(&self.child).try_wait()?;
            Ok(status.map(|status| {
                let exit = exit_of(&status);
                state.exit = Some(exit);
                state.collected = true;
                exit
            }))
        }
    }

    /// Ends the whole tree, collects the program and returns how it ended. Repeatable.
    pub fn kill(&self) -> io::Result<ProcessExit> {
        let mut state = lock(&self.state);
        if state.collected {
            // Collected already: on Windows closing the job still ends what the program left.
            #[cfg(windows)]
            tree::end_tree(&self.tree);
        } else {
            #[cfg(unix)]
            tree::end_group(self.pid);
            #[cfg(windows)]
            tree::end_tree(&self.tree);
            let status = lock(&self.child).wait()?;
            state.collected = true;
            // The session id may be another process's from now on: its guardian must not act.
            #[cfg(unix)]
            if let Some(watch) = lock(&self.watch).take() {
                watch.retire();
            }
            if state.exit.is_none() {
                state.exit = Some(exit_of(&status));
            }
        }
        state
            .exit
            .ok_or_else(|| io::Error::other("the program has no exit status"))
    }

    /// Delivers `signal` to the program on its terminal. `kill` ends the tree. On Unix any other signal goes
    /// to the program's process group. On Windows `terminate` ends the tree, `interrupt` types Ctrl-C (`0x03`)
    /// at the terminal, through `typed`, and every other signal is not delivered.
    pub fn signal(&self, signal: Signal, typed: &Mutex<Box<dyn Write + Send>>) -> Delivery {
        #[cfg(unix)]
        let _ = typed;
        if signal == Signal::Kill {
            return match self.kill() {
                Ok(_) => Delivery::tree_ended(None),
                Err(_) => Delivery::refused("the program could not be ended"),
            };
        }
        #[cfg(unix)]
        {
            if lock(&self.state).collected {
                return Delivery::refused("the program has ended");
            }
            if tree::send_group(self.pid, signal) {
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
                        "Windows has no SIGTERM: the program's tree was ended",
                    )),
                    Err(_) => Delivery::refused("the program could not be ended"),
                },
                Signal::Interrupt => {
                    if lock(&self.state).collected {
                        return Delivery::refused("the program has ended");
                    }
                    let mut writer = lock(typed);
                    match writer.write_all(&[0x03]).and_then(|()| writer.flush()) {
                        Ok(()) => Delivery::done(),
                        Err(_) => Delivery::refused("the terminal is closed"),
                    }
                }
                _ => Delivery::refused("Windows has no such signal"),
            }
        }
    }

    /// Tells the terminal its size changed.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        match lock(&self.master).as_ref() {
            Some(master) => master
                .resize(size(cols, rows))
                .map_err(|error| io_error(&*error)),
            None => Err(io::Error::other("the terminal is closed")),
        }
    }

    /// Closes the terminal, which ends the output of a pseudo-console that stays readable after
    /// its program has exited. Repeatable.
    pub fn close_terminal(&self) {
        drop(lock(&self.master).take());
    }
}

/// A cursor position query (DSR 6).
#[cfg(any(windows, test))]
pub const CURSOR_QUERY: &[u8] = b"\x1b[6n";

/// The answer to [`CURSOR_QUERY`]: row 1, column 1, where a fresh terminal's cursor is.
#[cfg(windows)]
pub const CURSOR_REPORT: &[u8] = b"\x1b[1;1R";

/// Removes cursor position queries from a terminal's output read in chunks, holding back the end of
/// a chunk that may be the start of one.
///
/// ConPTY sends one before its program's first output and holds that output until it is answered;
/// `portable-pty` creates every pseudo-console to ask, and nothing else is there to answer.
#[cfg(any(windows, test))]
#[derive(Debug, Default)]
pub struct CursorQueries {
    held: Vec<u8>,
}

#[cfg(any(windows, test))]
impl CursorQueries {
    /// The chunk with every whole query removed, and how many there were.
    pub fn filter(&mut self, chunk: &[u8]) -> (Vec<u8>, usize) {
        let mut bytes = std::mem::take(&mut self.held);
        bytes.extend_from_slice(chunk);
        let mut output = Vec::with_capacity(bytes.len());
        let mut asked = 0;
        let mut index = 0;
        while index < bytes.len() {
            let rest = &bytes[index..];
            if rest.starts_with(CURSOR_QUERY) {
                asked += 1;
                index += CURSOR_QUERY.len();
            } else if CURSOR_QUERY.starts_with(rest) {
                self.held = rest.to_vec();
                break;
            } else {
                output.push(bytes[index]);
                index += 1;
            }
        }
        (output, asked)
    }

    /// What was held back, once the output has ended.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered(chunks: &[&[u8]]) -> (Vec<u8>, usize) {
        let mut queries = CursorQueries::default();
        let mut output = Vec::new();
        let mut asked = 0;
        for chunk in chunks {
            let (out, n) = queries.filter(chunk);
            output.extend(out);
            asked += n;
        }
        output.extend(queries.finish());
        (output, asked)
    }

    #[test]
    fn a_cursor_query_is_counted_and_removed_even_across_chunks() {
        assert_eq!(
            filtered(&[b"\x1b[6n\x1b[?9001hready\r\n"]),
            (b"\x1b[?9001hready\r\n".to_vec(), 1)
        );
        assert_eq!(
            filtered(&[b"a\x1b", b"[", b"6", b"nb\x1b[6n"]),
            (b"ab".to_vec(), 2)
        );
    }

    #[test]
    fn other_sequences_and_a_held_tail_pass_through() {
        assert_eq!(
            filtered(&[b"\x1b[6m\x1b[66n", b"\x1b[6"]),
            (b"\x1b[6m\x1b[66n\x1b[6".to_vec(), 0)
        );
    }

    #[test]
    fn an_exit_code_with_the_high_bit_set_is_negative() {
        for (raw, code) in [
            (0, 0),
            (3, 3),
            (0xFFFF_FFFF, -1),
            (0xC000_013A, -1_073_741_510),
        ] {
            let exit = exit_of(&portable_pty::ExitStatus::with_exit_code(raw));
            assert_eq!(exit.code, Some(code), "{raw:#010X}");
        }
    }
}
