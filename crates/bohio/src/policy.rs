//! What each proven peer may do: the grants in `peers.cfg`.
//!
//! The trust store says who a peer is; this file says what it may do. A peer with no entry, or an
//! entry with no `allow`, gets nothing.
//!
//! ```toml
//! [peer.laptop]
//! allow = ["info", "fs.read", "exec"]
//! fs_roots = ["/srv/data"]
//!
//! [peer.laptop.exec]
//! programs = ["git", "make"]
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One thing a peer may be allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[non_exhaustive]
pub enum Permission {
    /// Ask for the host's capabilities.
    #[serde(rename = "info")]
    Info,
    /// Read files and directory listings under `fs_roots`.
    #[serde(rename = "fs.read")]
    FsRead,
    /// Change files and directories under `fs_roots`.
    #[serde(rename = "fs.write")]
    FsWrite,
    /// Run processes.
    #[serde(rename = "exec")]
    Exec,
    /// Open interactive shells.
    #[serde(rename = "shell")]
    Shell,
    /// Manage the host's peers.
    #[serde(rename = "admin")]
    Admin,
}

impl Permission {
    /// The name `peers.cfg` uses.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::Exec => "exec",
            Self::Shell => "shell",
            Self::Admin => "admin",
        }
    }
}

/// What one peer may do.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Grant {
    /// The permissions held. Empty: nothing.
    allow: BTreeSet<Permission>,
    /// The directories file operations are confined to. Empty: no file operation is allowed.
    fs_roots: Vec<PathBuf>,
    /// Limits on `exec`.
    exec: ExecGrant,
}

/// `[peer.<id>.exec]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExecGrant {
    /// The programs the peer may run, each named exactly as the peer asks for it. Empty: any
    /// program, and explicit shell scripts. Non-empty: only these, and no shell script.
    programs: Vec<String>,
}

impl Grant {
    /// A grant of nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// A grant of `permissions` with no roots and no program list.
    pub fn allowing(permissions: impl IntoIterator<Item = Permission>) -> Self {
        Self {
            allow: permissions.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Confines file operations to `roots`.
    #[must_use]
    pub fn with_fs_roots(mut self, roots: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.fs_roots = roots.into_iter().map(Into::into).collect();
        self
    }

    /// Limits `exec` to `programs`, each named exactly as the peer asks for it.
    #[must_use]
    pub fn with_programs(mut self, programs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.exec.programs = programs.into_iter().map(Into::into).collect();
        self
    }

    /// Whether `permission` is held.
    pub fn allows(&self, permission: Permission) -> bool {
        self.allow.contains(&permission)
    }

    /// The permissions held, in order.
    pub fn permissions(&self) -> impl Iterator<Item = Permission> + '_ {
        self.allow.iter().copied()
    }

    /// The directories file operations are confined to.
    pub fn fs_roots(&self) -> &[PathBuf] {
        &self.fs_roots
    }

    /// Whether the peer may run `program`, named exactly as it asked.
    pub fn permits_program(&self, program: &str) -> bool {
        self.exec.programs.is_empty() || self.exec.programs.iter().any(|p| p == program)
    }

    /// Whether the peer may hand an explicit script to a shell: only without a program list, since
    /// a shell runs whatever it is given.
    pub fn permits_shell_script(&self) -> bool {
        self.exec.programs.is_empty()
    }
}

/// Why `peers.cfg` could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PolicyError {
    /// The file could not be read.
    #[error("{path}: cannot be read: {reason}")]
    Read {
        /// The file.
        path: String,
        /// The operating system's reason.
        reason: String,
    },
    /// Bad TOML, an unknown key or an unknown permission.
    #[error("{path}:{line}:{column}: {message}")]
    Parse {
        /// The file, or `<text>`.
        path: String,
        /// The line of the error, from 1.
        line: usize,
        /// The column of the error, from 1.
        column: usize,
        /// What is wrong, without the line's content.
        message: String,
    },
}

/// Every peer's grant, by the peer's id.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Policies {
    peer: BTreeMap<String, Grant>,
}

impl Policies {
    /// Reads `peers.cfg` text. `path` only labels errors.
    ///
    /// # Errors
    ///
    /// [`PolicyError::Parse`] for bad TOML, an unknown key or an unknown permission.
    pub fn from_toml(text: &str, path: &str) -> Result<Self, PolicyError> {
        toml::from_str(text).map_err(|error| {
            let (line, column) = error
                .span()
                .map_or((1, 1), |span| crate::config::position(text, span.start));
            PolicyError::Parse {
                path: path.to_owned(),
                line,
                column,
                message: error.message().to_owned(),
            }
        })
    }

    /// Reads the policy file at `path`; a missing file is an empty policy.
    ///
    /// # Errors
    ///
    /// [`PolicyError::Read`] when the file exists and cannot be read, else as
    /// [`Policies::from_toml`].
    pub fn load(path: &Path) -> Result<Self, PolicyError> {
        let label = path.display().to_string();
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text, &label),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(PolicyError::Read {
                path: label,
                reason: error.kind().to_string(),
            }),
        }
    }

    /// What `peer` may do: its grant, or nothing.
    pub fn grant_for(&self, peer: &str) -> Grant {
        self.peer.get(peer).cloned().unwrap_or_default()
    }

    /// The ids with an entry, in order.
    pub fn peers(&self) -> impl Iterator<Item = &str> {
        self.peer.keys().map(String::as_str)
    }
}
