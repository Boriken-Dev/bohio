//! What each peer may do: the `[peer.<id>]` tables of `bohio.cfg`.
//!
//! A table, even an empty one, is what accepts a key; `allow` says what the key may do. A peer with
//! no `allow` gets nothing.
//!
//! ```toml
//! [peer.<id>]
//! allow = ["info", "fs.read", "exec"]
//! fs_roots = ["/srv/data"]
//!
//! [peer.<id>.exec]
//! programs = ["git", "make"]
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One thing a peer may be allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
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
    /// Every permission, in order.
    pub const ALL: [Self; 6] = [
        Self::Info,
        Self::FsRead,
        Self::FsWrite,
        Self::Exec,
        Self::Shell,
        Self::Admin,
    ];

    /// The permission named `name` as `bohio.cfg` writes it.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|permission| permission.name() == name)
    }

    /// The name `bohio.cfg` uses.
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
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Grant {
    /// The permissions held. Empty: nothing.
    allow: BTreeSet<Permission>,
    /// The directories file operations are confined to. Empty: no file operation is allowed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fs_roots: Vec<PathBuf>,
    /// Limits on `exec`.
    #[serde(skip_serializing_if = "ExecGrant::is_unlimited")]
    exec: ExecGrant,
}

/// `[peer.<id>.exec]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExecGrant {
    /// The programs the peer may run, each named exactly as the peer asks for it. Empty: any
    /// program, and explicit shell scripts. Non-empty: only these, found on the host's own
    /// `PATH` (a name with a directory must be absolute), with no shell script, and the request
    /// may not set or remove `PATH` or clear the environment. The list limits which program
    /// starts, not what it does.
    programs: Vec<String>,
    /// Under a program list, the environment variables a request may set or remove, each named
    /// exactly (without case on Windows); any other is refused. `PATH` is refused like any other
    /// unless it is listed, and then a listed program that starts others runs what the peer's
    /// `PATH` finds. A variable is as strong as an argument: `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`,
    /// `BASH_ENV` or `GIT_SSH_COMMAND` make a listed program run code the peer chose. Without a
    /// program list a peer may set any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    env: Vec<String>,
}

impl ExecGrant {
    fn is_unlimited(&self) -> bool {
        self.programs.is_empty() && self.env.is_empty()
    }
}

/// Whether two environment variable names are one name on this host.
fn same_variable(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
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

    /// Limits `exec` to `programs`, each named exactly as the peer asks for it and started from
    /// the path the host finds it at.
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

    /// Under a program list, lets a request set or remove the variables `names` and no others.
    #[must_use]
    pub fn with_env(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.exec.env = names.into_iter().map(Into::into).collect();
        self
    }

    /// The variables a request may change under a program list.
    pub fn env_names(&self) -> &[String] {
        &self.exec.env
    }

    /// Whether a request may make the environment changes `env`: always without a program list;
    /// under one, only variables the grant names (`PATH` too, when it names it), and never from an
    /// empty environment.
    pub fn permits_env(&self, env: &bohio_proto::exec::Env) -> bool {
        if self.exec.programs.is_empty() {
            return true;
        }
        let allowed = |name: &String| {
            self.exec
                .env
                .iter()
                .any(|listed| same_variable(listed, name))
        };
        !env.clear && env.set.keys().all(allowed) && env.remove.iter().all(allowed)
    }

    /// Whether the peer may run `program`, named exactly as it asked.
    pub fn permits_program(&self, program: &str) -> bool {
        self.exec.programs.is_empty() || self.exec.programs.iter().any(|p| p == program)
    }

    /// Whether the grant lists programs: the peer may then start only those, from the host's own
    /// search path.
    pub fn restricts_programs(&self) -> bool {
        !self.exec.programs.is_empty()
    }

    /// Whether the peer may hand an explicit script to a shell: only without a program list, since
    /// a shell runs whatever it is given.
    pub fn permits_shell_script(&self) -> bool {
        self.exec.programs.is_empty()
    }
}

/// The peers a configuration lists, by the peer's id: `[peer.<id>]` in `bohio.cfg`. A key is accepted
/// when its table is here, even an empty one, and what it may do is the table's grant.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Peers(BTreeMap<String, Grant>);

impl Peers {
    /// What `peer` may do: its grant, or nothing.
    pub fn grant_for(&self, peer: &str) -> Grant {
        self.0.get(peer).cloned().unwrap_or_default()
    }

    /// Whether `peer` has a table, even one that grants nothing: whether its key is accepted.
    pub fn has(&self, peer: &str) -> bool {
        self.0.contains_key(peer)
    }

    /// The ids with a table, in order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// How many peers there are.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there is no peer.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Gives `peer` exactly `grant`, replacing any table it had.
    pub fn set(&mut self, peer: &str, grant: Grant) {
        self.0.insert(peer.to_owned(), grant);
    }

    /// Removes `peer`'s table; `true` when it had one.
    pub fn remove(&mut self, peer: &str) -> bool {
        self.0.remove(peer).is_some()
    }
}
