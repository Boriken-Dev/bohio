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
//!
//! [peer.<id>.forward]
//! connect = ["db.lan:5432", "10.0.0.0/8:22", "unix:/run/docker.sock"]
//! listen = ["127.0.0.1:0", "127.0.0.1:18090-18099", "unix:/run/user/*/app.sock"]
//! # What this node does toward the peer, kept up across link losses (`connect` and `listen` above are what the peer may do here):
//! local = ["127.0.0.1:5432=db.lan:5432"]
//! remote = ["127.0.0.1:0=127.0.0.1:8080"]
//! ```

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::DialConfig;

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
    /// Have the host connect to the targets `[peer.<id>.forward] connect` lists: TCP, Unix sockets and Windows
    /// named pipes, from the host's own network position.
    #[serde(rename = "forward.connect")]
    ForwardConnect,
    /// See the desktop: snapshots, and the wait for the screen to settle. Everything on the screen is readable: passwords
    /// shown in clear, one-time codes, messages.
    #[serde(rename = "gui.view")]
    GuiView,
    /// Type and click on the desktop, with the authority of the person who owns it: more than `exec`'s program list or
    /// `fs_roots` give.
    #[serde(rename = "gui.input")]
    GuiInput,
    /// Read and write the desktop's clipboard text, beside `gui.view` or `gui.input`. A password manager's copy is in it.
    #[serde(rename = "gui.clipboard")]
    GuiClipboard,
    /// Have the host listen where `[peer.<id>.forward] listen` allows (a TCP address, a Unix socket or a Windows named
    /// pipe) and hand each connection it takes to the peer.
    #[serde(rename = "forward.listen")]
    ForwardListen,
}

impl Permission {
    /// Every permission, in order.
    pub const ALL: [Self; 11] = [
        Self::Info,
        Self::FsRead,
        Self::FsWrite,
        Self::Exec,
        Self::Shell,
        Self::Admin,
        Self::ForwardConnect,
        Self::GuiView,
        Self::GuiInput,
        Self::GuiClipboard,
        Self::ForwardListen,
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
            Self::ForwardConnect => "forward.connect",
            Self::GuiView => "gui.view",
            Self::GuiInput => "gui.input",
            Self::GuiClipboard => "gui.clipboard",
            Self::ForwardListen => "forward.listen",
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
    /// Where `forward.connect` reaches and what `forward.listen` binds.
    #[serde(skip_serializing_if = "ForwardGrant::is_empty")]
    forward: ForwardGrant,
}

/// `[peer.<id>.forward]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ForwardGrant {
    /// The targets the peer may have the host connect to, as rules: `HOST:PORT` where HOST is an exact name, `*.suffix`,
    /// `*`, an address or a CIDR network and PORT is `N`, `A-B` or `*` (`coqui_link::forward::Rule`); `unix:PATH` and
    /// `pipe:NAME`, exactly or with `*` and `?`, which match within one path component. The peer needs `forward.connect`
    /// as well as a rule that matches; an empty list allows nothing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    connect: Vec<String>,
    /// What the peer may have the host bind, as rules: `BIND:PORT` where BIND is an address, a CIDR network or `*` and PORT
    /// is `N`, `A-B` or `*` (`0` asks the host for a free port and is allowed only when a rule names it, or `*`);
    /// `unix:PATH` and `pipe:NAME` as in `connect`. The peer needs `forward.listen` as well; an empty list allows nothing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    listen: Vec<String>,
}

impl ForwardGrant {
    fn is_empty(&self) -> bool {
        self.connect.is_empty() && self.listen.is_empty()
    }

    /// The rule texts, in order.
    pub fn connect(&self) -> &[String] {
        &self.connect
    }

    /// The `listen` rule texts, in order.
    pub fn listen(&self) -> &[String] {
        &self.listen
    }
}

/// `[peer.<id>.forward]` as a table holds it: what the peer may do here (the grant's `connect` and `listen`) and what this node
/// keeps up toward the peer (`local` and `remote`, which are not part of a grant).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ForwardTable {
    connect: Vec<String>,
    listen: Vec<String>,
    local: Vec<String>,
    remote: Vec<String>,
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

    /// Lets the peer have the host connect to what `rules` match; the peer also needs [`Permission::ForwardConnect`].
    #[must_use]
    pub fn with_connect(mut self, rules: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.forward.connect = rules.into_iter().map(Into::into).collect();
        self
    }

    /// Lets the peer have the host bind what `rules` match; the peer also needs [`Permission::ForwardListen`].
    #[must_use]
    pub fn with_listen(mut self, rules: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.forward.listen = rules.into_iter().map(Into::into).collect();
        self
    }

    /// The `[forward]` part of the grant.
    pub const fn forward(&self) -> &ForwardGrant {
        &self.forward
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

/// One `[peer.<id>]` table: what the peer may do (`allow`, `fs_roots`, `[exec]`) and, if this node dials it,
/// where (`endpoint`, `[dial]`).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PeerTable {
    allow: BTreeSet<Permission>,
    fs_roots: Vec<PathBuf>,
    exec: ExecGrant,
    forward: ForwardTable,
    /// Where this node dials the peer, any endpoint `bohio remote` takes. The table's name pins the key
    /// that must answer there. Absent: this node does not dial the peer.
    endpoint: Option<String>,
    dial: DialConfig,
}

impl PeerTable {
    /// What the peer may do.
    pub fn grant(&self) -> Grant {
        Grant {
            allow: self.allow.clone(),
            fs_roots: self.fs_roots.clone(),
            exec: self.exec.clone(),
            forward: ForwardGrant {
                connect: self.forward.connect.clone(),
                listen: self.forward.listen.clone(),
            },
        }
    }

    /// The `-L` forwards this node keeps toward the peer (`[peer."<id>".forward] local`), as written: `LISTEN=TARGET`.
    pub fn forward_local(&self) -> &[String] {
        &self.forward.local
    }

    /// The `-R` forwards this node keeps toward the peer (`[peer."<id>".forward] remote`), as written: `LISTEN=LOCAL`.
    pub fn forward_remote(&self) -> &[String] {
        &self.forward.remote
    }

    /// Where this node dials the peer, when it does.
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// How this node dials the peer.
    pub const fn dial(&self) -> &DialConfig {
        &self.dial
    }
}

/// The peers a configuration lists, by the peer's id: `[peer.<id>]` in `bohio.cfg`. A key is accepted
/// when its table is here, even an empty one, and what it may do is the table's grant.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(transparent)]
pub struct Peers(BTreeMap<String, PeerTable>);

impl Peers {
    /// What `peer` may do: its grant, or nothing.
    pub fn grant_for(&self, peer: &str) -> Grant {
        self.0.get(peer).map(PeerTable::grant).unwrap_or_default()
    }

    /// `peer`'s table, when it has one.
    pub fn table(&self, peer: &str) -> Option<&PeerTable> {
        self.0.get(peer)
    }

    /// The peers this node dials: each id with its endpoint text and dial settings, in id order.
    pub fn dials(&self) -> impl Iterator<Item = (&str, &str, &DialConfig)> {
        self.0.iter().filter_map(|(name, table)| {
            table
                .endpoint()
                .map(|endpoint| (name.as_str(), endpoint, table.dial()))
        })
    }

    /// The peers whose table keeps forwards up toward them (`local` or `remote` under `[forward]`): each id with its table, in
    /// id order.
    pub fn declared(&self) -> impl Iterator<Item = (&str, &PeerTable)> {
        self.0
            .iter()
            .filter(|(_, table)| {
                !table.forward_local().is_empty() || !table.forward_remote().is_empty()
            })
            .map(|(name, table)| (name.as_str(), table))
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
}
