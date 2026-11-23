//! What a host reports about itself: the answer to `info`.
//!
//! Every field has a default and unknown fields are ignored, so a client works with a host that
//! reports less or more than it expects.

use serde::{Deserialize, Serialize};

use crate::compress::{Codec, Compression};
use crate::forward::ForwardInfo;
use crate::gui::GuiInfo;
use crate::hash::Algorithm;
use crate::shell::ShellKind;
use crate::signal::Signal;

/// The protocol version this crate speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// A host's capabilities.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
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
    /// Detached sessions.
    pub sessions: DetachedInfo,
    /// Bounds the host applies.
    pub limits: Limits,
    /// What the host does to move and compare bytes.
    pub transfer: TransferInfo,
    /// Forwarding.
    pub forward: ForwardInfo,
    /// The desktop: screen, keyboard and mouse.
    pub gui: GuiInfo,
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
    /// The signals the host delivers to a running program (`signal` frames); the others are answered `delivered: false`.
    pub signals: Vec<Signal>,
}

/// Interactive shell sessions.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionInfo {
    /// Whether the host opens shell sessions.
    pub enabled: bool,
    /// The signals the host delivers to a program on a terminal.
    pub signals: Vec<Signal>,
}

/// Detached sessions: programs that outlive their stream.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DetachedInfo {
    /// Whether the host runs sessions (a peer still needs `session`).
    pub enabled: bool,
    /// The output bytes kept for a session that asks for no size.
    pub scrollback: u64,
    /// The most output bytes a session may ask to keep.
    pub max_scrollback: u64,
    /// Milliseconds a detached session nobody attaches to is kept.
    pub linger_ms: u64,
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
    /// The compression algorithms the host offers, each with the levels it takes; empty when it
    /// compresses nothing (a build without zstd still offers `lz4` and `deflate`).
    pub compression: Vec<CompressionInfo>,
    /// What `auto` means on this host.
    pub compression_default: Option<Compression>,
    /// Whether the host compresses a whole range first when a read says `prepare`.
    pub prepared: bool,
    /// What the host offers for deltas and block signatures.
    pub delta: DeltaInfo,
    /// Whether the host serves `bohio-walk/1`.
    pub walk: bool,
    /// The bounds `bohio-walk/1` runs under.
    pub walk_limits: WalkLimits,
    /// Whether reads and writes take bundles of many files on one stream (`items`).
    pub bundles: bool,
    /// Whether a `replace` write may ask to `resume`: the host keeps an abandoned one for a while.
    pub resume: bool,
}

/// What the host offers for deltas (`bohio-read/1` with `delta`, `bohio-write/1` in mode `delta`, and block
/// hashes from `bohio-hash/1`). A host that serves none reports `weak` empty.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeltaInfo {
    /// The weak checksums it computes: `rsync`. Empty when it serves no deltas.
    pub weak: Vec<String>,
    /// The smallest block size it chooses for a signature, and the largest it accepts or chooses.
    pub min_block: u64,
    /// See `min_block`.
    pub max_block: u64,
    /// The size of file from which a client's `auto` uses a delta.
    pub min_size: u64,
    /// The most bytes of signature a read's client may send.
    pub max_signature_bytes: u64,
}

/// One compression algorithm a host offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionInfo {
    /// Which algorithm.
    pub algorithm: Codec,
    /// The levels it takes, after the host's own cap. `0` and `0` for an algorithm without levels.
    #[serde(default)]
    pub levels: Levels,
    /// The level used when a request names none.
    #[serde(default)]
    pub default_level: i32,
    /// The largest window the host uses or accepts, as a power of two; `0` for an algorithm without
    /// a configurable window.
    #[serde(default)]
    pub window_log_max: u32,
}

/// A range of compression levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Levels {
    /// The lowest (fastest) level.
    pub min: i32,
    /// The highest level the host allows.
    pub max: i32,
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

/// The bounds of `bohio-walk/1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WalkLimits {
    /// The most walk streams running at once on the host; a stream past it is `busy`. `0` when the host
    /// serves none.
    pub max_streams: u64,
    /// The most entries one walk stream sends; past it the walk ends with an item error. `0` is no limit.
    pub max_entries: u64,
}
