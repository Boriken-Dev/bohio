//! Signals a client sends to a running program, on a `bohio-exec/1`, `bohio-shell/1` or attached stream.
//!
//! The client frame is tag `0x15` with a CBOR [`SignalRequest`]; the host answers every one with a
//! `0x08` [`Signalled`] frame saying whether it was delivered. What each signal does to a program is
//! the operating system's: on Unix it is the signal sent to the program's process group; on Windows
//! the host delivers what that system has (`info.exec.signals` and `info.shell.signals` list it).

use serde::{Deserialize, Serialize};

/// A signal a client asks the host to deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Signal {
    /// The program is asked to stop what it is doing (`SIGINT`, Ctrl-C).
    Interrupt,
    /// The program is asked to end and may clean up (`SIGTERM`).
    Terminate,
    /// The program's terminal went away (`SIGHUP`).
    Hangup,
    /// The program is asked to end and dump (`SIGQUIT`).
    Quit,
    /// `SIGUSR1`.
    User1,
    /// `SIGUSR2`.
    User2,
    /// The whole tree is ended at once and cannot object (`SIGKILL`).
    Kill,
}

impl Signal {
    /// Every signal, in order.
    pub const ALL: [Self; 7] = [
        Self::Interrupt,
        Self::Terminate,
        Self::Hangup,
        Self::Quit,
        Self::User1,
        Self::User2,
        Self::Kill,
    ];

    /// The name a command line and a configuration use.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Terminate => "terminate",
            Self::Hangup => "hangup",
            Self::Quit => "quit",
            Self::User1 => "user1",
            Self::User2 => "user2",
            Self::Kill => "kill",
        }
    }

    /// The signal named `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|signal| signal.name() == name)
    }
}

impl core::fmt::Display for Signal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// The body of the client's `0x15` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalRequest {
    /// The signal to deliver.
    pub signal: Signal,
}

/// The body of the host's `0x08` answer to a signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signalled {
    /// The signal that was asked for.
    pub signal: Signal,
    /// Whether the host delivered it. `false` is not an error: this system has no such signal for this
    /// program, or the program had already ended.
    pub delivered: bool,
    /// Why it was not delivered, or what delivering it meant on this system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
