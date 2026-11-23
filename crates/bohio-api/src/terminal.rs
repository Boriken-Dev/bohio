// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The terminal kind: a console a peer types at and reads from.
//!
//! A pseudo-terminal that starts a program and a serial line that is a console are the same kind. What differs is
//! `describe`: a serial console lacks `resize`, `signals`, `exit_status` and `programs`.

use guatiao::library::{Object, ProviderError};

use crate::model::{Delivery, SourceInfo, TerminalExit, TerminalOpen};
use crate::unsupported;

/// A source of terminals.
#[guatiao::kind(name = "bohio_terminal")]
pub trait TerminalSource: Send + Sync {
    /// What this source is and can do now; asked at every `info`, never cached by the host.
    ///
    /// # Errors
    ///
    /// Only a source that cannot even describe itself.
    fn describe(&self) -> Result<SourceInfo, ProviderError>;

    /// Opens a terminal. The host has already decided the grant: the request holds what the source needs and nothing
    /// a peer may not ask.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND` (the program), `BAD_VALUE` (a request the source cannot serve, as a program for a console),
    /// `GONE` (the source is unavailable), `BUSY`.
    fn open(&self, request: TerminalOpen) -> Result<Object<dyn Terminal>, ProviderError>;
}

/// One open terminal. Every method takes `&self`: the host reads on one thread and writes on another.
#[guatiao::kind(object, name = "bohio_terminal_session")]
pub trait Terminal: Send + Sync {
    /// Reads output into `dst`, waiting up to `wait_ms` for some. Answers the bytes read; `0` when none came in
    /// time; `-1` when the terminal has ended and nothing more will come.
    ///
    /// # Errors
    ///
    /// `GONE` when the device went away.
    fn read(&self, dst: &mut [u8], wait_ms: u64) -> Result<i64, ProviderError>;

    /// Types `data` at the terminal. Answers the bytes taken, all of them unless the terminal has ended.
    ///
    /// # Errors
    ///
    /// `GONE` when the device went away.
    fn write(&self, data: &[u8]) -> Result<u64, ProviderError>;

    /// Ends the terminal's program and everything it started and closes the terminal. A console that starts no
    /// program has nothing to end and only lets go of its attach. Answers how the program ended.
    ///
    /// # Errors
    ///
    /// Anything the source cannot end.
    fn kill(&self) -> Result<TerminalExit, ProviderError>;

    /// How the program ended, `None` while it runs. A source with no program never ends it by itself.
    ///
    /// # Errors
    ///
    /// `GONE` when the device went away.
    fn exit(&self) -> Result<Option<TerminalExit>, ProviderError>;

    /// The program's process id on the host, when it has one.
    ///
    /// # Errors
    ///
    /// Never, by default.
    fn pid(&self) -> Result<Option<u64>, ProviderError> {
        Ok(None)
    }

    /// Changes the terminal's size.
    ///
    /// # Errors
    ///
    /// `UNSUPPORTED` when the source has no size (it lacks `resize`).
    fn resize(&self, cols: u16, rows: u16) -> Result<(), ProviderError> {
        let _ = (cols, rows);
        Err(unsupported("this terminal has no size to change"))
    }

    /// Delivers a signal by name (`interrupt`, `terminate`, `kill`, `hangup`, `quit`, `user1`, `user2`).
    ///
    /// # Errors
    ///
    /// `UNSUPPORTED` when the source has no program to signal (it lacks `signals`).
    fn signal(&self, name: &str) -> Result<Delivery, ProviderError> {
        let _ = name;
        Err(unsupported("this terminal has no program to signal"))
    }
}
