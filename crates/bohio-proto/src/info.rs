//! What a host reports about itself: the answer to `info`.
//!
//! Every field has a default and unknown fields are ignored, so a client works with a host that
//! reports less or more than it expects.

use serde::{Deserialize, Serialize};

use crate::hash::Algorithm;
use crate::shell::ShellKind;

/// The protocol version this crate speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// A host's capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Info {
    /// The protocol version the host speaks.
    pub protocol: u32,
    /// The program serving.
    pub service: Service,
    /// The host's identity once its link has proven one; absent before.
    pub host_id: Option<String>,
    /// The operating system, as `std::env::consts::OS` names it.
    pub os: String,
    /// The processor architecture, as `std::env::consts::ARCH` names it.
    pub arch: String,
    /// The shells found on the host.
    pub shells: Vec<ShellInfo>,
    /// Filesystem access.
    pub filesystem: FilesystemInfo,
    /// One-shot process execution.
    pub exec: ExecInfo,
    /// Interactive shells.
    pub shell: SessionInfo,
    /// Bounds the host applies.
    pub limits: Limits,
    /// What the host does to move and compare bytes.
    pub transfer: TransferInfo,
}

/// The serving program.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Service {
    /// The name the host's configuration gives it.
    pub name: String,
    /// The program's version.
    pub version: String,
}

/// A shell found on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellInfo {
    /// Which shell.
    pub kind: ShellKind,
    /// Where it was found.
    pub path: String,
}

/// What the filesystem offers on this host, so a client does not assume POSIX on Windows.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesystemInfo {
    /// Whether the host serves filesystem calls.
    pub enabled: bool,
    /// Symbolic links can be made and read.
    pub symlinks: bool,
    /// Permissions are Unix mode bits.
    pub unix_modes: bool,
    /// Files carry Windows attributes.
    pub windows_attributes: bool,
    /// Names differing only in case are different files.
    pub case_sensitive: bool,
    /// The path separator of the host's native text.
    pub separator: String,
}

/// One-shot process execution.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecInfo {
    /// Whether the host runs processes.
    pub enabled: bool,
}

/// Interactive shell sessions.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionInfo {
    /// Whether the host opens shell sessions.
    pub enabled: bool,
}

/// Bounds the host applies; a missing one means the host reports none.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Concurrent streams per peer.
    pub max_streams: Option<u64>,
    /// Concurrent one-shot processes.
    pub max_processes: Option<u64>,
    /// Concurrent shell sessions.
    pub max_sessions: Option<u64>,
}

/// How the host moves and compares bytes. A client that finds something missing does not use it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferInfo {
    /// The digest algorithms the host computes for `bohio-hash/1`, and for the `hashes` and `expect`
    /// of reads and writes: empty when it serves none.
    pub hashes: Vec<Algorithm>,
    /// The bounds `bohio-hash/1` runs under.
    pub hash_limits: HashLimits,
}

/// The bounds of `bohio-hash/1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HashLimits {
    /// The most hash streams running at once on the host; a stream past it is `busy`. `0` when the
    /// host serves none.
    pub max_streams: u64,
    /// The most bytes a second one stream hashes; `0` is no limit.
    pub rate: u64,
}
