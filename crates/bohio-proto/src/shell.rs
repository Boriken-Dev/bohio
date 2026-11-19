//! Shell kinds, shared by the host's capabilities, explicit shell commands and shell sessions.

use serde::{Deserialize, Serialize};

/// A shell a host may offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShellKind {
    /// POSIX `sh`.
    Sh,
    /// GNU `bash`.
    Bash,
    /// `zsh`.
    Zsh,
    /// The Windows command interpreter, `cmd`.
    Cmd,
    /// Windows PowerShell, `powershell`.
    Powershell,
    /// PowerShell 7 and later, `pwsh`.
    Pwsh,
    /// A kind a newer peer knows.
    #[serde(other)]
    Other,
}

impl ShellKind {
    /// Every kind this version knows, in the order a host probes for them.
    pub const KNOWN: [Self; 6] = [
        Self::Sh,
        Self::Bash,
        Self::Zsh,
        Self::Cmd,
        Self::Powershell,
        Self::Pwsh,
    ];

    /// The program name looked up on the host's `PATH`; empty for [`ShellKind::Other`].
    pub const fn program(self) -> &'static str {
        match self {
            Self::Sh => "sh",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Cmd => "cmd",
            Self::Powershell => "powershell",
            Self::Pwsh => "pwsh",
            Self::Other => "",
        }
    }
}
