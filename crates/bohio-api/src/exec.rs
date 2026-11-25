// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The exec kind: programs a source runs for a peer, with their input and output as streams.
//!
//! The node's own operating system and a machine reached over SSH are the same kind. The host decides the grant
//! (the program list, the environment a peer may change, the shell scripts it may run) before a source sees the
//! request; a source only starts what it is given and says in `describe` what it cannot do.

use guatiao::library::{Object, ProviderError};

use crate::model::{Delivery, ExecStart, SourceInfo, TerminalExit};
use crate::unsupported;

/// The `stream` argument of [`Process::read`] that reads standard output.
pub const STDOUT: u8 = 1;
/// The `stream` argument of [`Process::read`] that reads standard error.
pub const STDERR: u8 = 2;

/// A source of processes.
#[guatiao::kind(name = "bohio_exec")]
pub trait ExecSource: Send + Sync {
    /// What this source is and can do now; asked at every `info`, never cached by the host.
    ///
    /// # Errors
    ///
    /// Only a source that cannot even describe itself.
    fn describe(&self) -> Result<SourceInfo, ProviderError>;

    /// Starts a program in a tree of its own. The host has already decided the grant.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND` (the program or the working directory), `BAD_VALUE` (a request the source cannot serve), `GONE`
    /// (the source is unavailable), `BUSY` (the source is running as many programs as it may).
    fn start(&self, request: ExecStart) -> Result<Object<dyn Process>, ProviderError>;
}

/// One running program. Every method takes `&self`: the host reads both outputs, writes the input and watches the
/// exit from threads of its own.
#[guatiao::kind(object, name = "bohio_exec_process")]
pub trait Process: Send + Sync {
    /// Reads `stream` ([`STDOUT`] or [`STDERR`]) into `dst`, waiting up to `wait_ms` for some. Answers the bytes
    /// read; `0` when none came in time; `-1` when that stream has ended and nothing more will come. A source that
    /// merges the two answers everything on [`STDOUT`] and ends [`STDERR`] at once.
    ///
    /// # Errors
    ///
    /// `GONE` when the source went away.
    fn read(&self, stream: u8, dst: &mut [u8], wait_ms: u64) -> Result<i64, ProviderError>;

    /// Writes `data` to standard input. Answers the bytes taken, all of them unless the input has been closed.
    ///
    /// # Errors
    ///
    /// `GONE` when the input is closed or the source went away.
    fn write(&self, data: &[u8]) -> Result<u64, ProviderError>;

    /// Ends standard input.
    ///
    /// # Errors
    ///
    /// `GONE` when the source went away.
    fn close_input(&self) -> Result<(), ProviderError>;

    /// How the program ended, waiting up to `wait_ms` for it; `None` while it runs.
    ///
    /// # Errors
    ///
    /// `GONE` when the source went away.
    fn wait(&self, wait_ms: u64) -> Result<Option<TerminalExit>, ProviderError>;

    /// Ends the program and everything it started, and answers how it ended. Calling it again answers the same.
    ///
    /// # Errors
    ///
    /// Anything the source cannot end.
    fn kill(&self) -> Result<TerminalExit, ProviderError>;

    /// The program's process id where it runs, when there is one.
    ///
    /// # Errors
    ///
    /// Never, by default.
    fn pid(&self) -> Result<Option<u64>, ProviderError> {
        Ok(None)
    }

    /// Delivers a signal by name (`interrupt`, `terminate`, `kill`, `hangup`, `quit`, `user1`, `user2`).
    ///
    /// # Errors
    ///
    /// `UNSUPPORTED` when the source cannot signal (it lacks `signals`).
    fn signal(&self, name: &str) -> Result<Delivery, ProviderError> {
        let _ = name;
        Err(unsupported("this source cannot signal a program"))
    }
}
