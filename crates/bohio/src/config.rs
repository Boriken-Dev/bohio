//! `bohio.cfg`: the host's configuration, read from TOML as layers.
//!
//! Every key is optional and has a documented default. A key this version does not know is an
//! error naming it, so a misspelled setting never looks like it worked. [`Layers`] reads the system
//! file and the user's as one configuration, each merged over the one before; a peer is a `[peer.<id>]`
//! table in it.

mod layers;

pub use layers::{Layered, Layers};

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::{self, Deserializer};

use crate::policy::Peers;

/// The environment the data directory and the host's programs are looked for in, captured by the
/// binary so the library reads none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    /// Whether to use Windows locations (`%LOCALAPPDATA%`) instead of Unix ones.
    pub windows: bool,
    /// `LOCALAPPDATA` (Windows): the user's data directory.
    pub local_app_data: Option<PathBuf>,
    /// `XDG_DATA_HOME` (Unix).
    pub xdg_data_home: Option<PathBuf>,
    /// `HOME` (Unix).
    pub home: Option<PathBuf>,
    /// `PATH`: where programs and shells are looked for.
    pub path: Option<OsString>,
}

impl Environment {
    /// The directory holding the host's identity and waiting list when the configuration names none:
    /// under the user's data directory.
    pub fn default_data_dir(&self) -> PathBuf {
        let base = if self.windows {
            self.local_app_data.clone()
        } else {
            self.xdg_data_home
                .clone()
                .or_else(|| self.home.as_ref().map(|h| h.join(".local").join("share")))
        };
        base.unwrap_or_else(|| PathBuf::from(".")).join("bohio")
    }
}

/// Why a configuration could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file could not be read.
    Read {
        /// The file.
        path: String,
        /// The operating system's reason.
        reason: String,
    },
    /// The file is not valid TOML, or holds a key this version does not know, or a wrong type.
    Parse {
        /// The file, or `<text>`.
        path: String,
        /// The line of the error, from 1.
        line: usize,
        /// The column of the error, from 1.
        column: usize,
        /// The key, as written in the file, when the error is about one.
        key: Option<String>,
        /// What is wrong, never a value of the file.
        message: String,
    },
    /// A value is out of range.
    Invalid {
        /// The file, or `<text>`: the highest layer that sets the key.
        path: String,
        /// The key, as written in the file.
        key: String,
        /// What is wrong with it.
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, reason } => write!(f, "{path}: cannot be read: {reason}"),
            Self::Parse {
                path,
                line,
                column,
                key: Some(key),
                message,
            } => write!(f, "{path}:{line}:{column}: `{key}` {message}"),
            Self::Parse {
                path,
                line,
                column,
                key: None,
                message,
            } => write!(f, "{path}:{line}:{column}: {message}"),
            Self::Invalid { path, key, message } => write!(f, "{path}: `{key}` {message}"),
        }
    }
}

impl core::error::Error for ConfigError {}

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// `[service]`.
    pub service: ServiceConfig,
    /// `[[listen]]`: where the host accepts links.
    pub listen: Vec<ListenConfig>,
    /// `[link]`.
    pub link: LinkConfig,
    /// `[filesystem]`.
    pub filesystem: FilesystemConfig,
    /// `[execution]`.
    pub execution: ExecutionConfig,
    /// `[shell]`.
    pub shell: ShellConfig,
    /// `[control]`: the node's local endpoint.
    pub control: ControlConfig,
    /// `[peer.<id>]`: each accepted peer and what it may do. A key is accepted while any layer holds
    /// its table, even an empty one.
    pub peer: Peers,
}

/// `[service]`: who the host is.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServiceConfig {
    /// A name shown in `info` and logs. Default `bohio`.
    pub name: String,
    /// The directory holding the identity and the waiting list. Default: `bohio`
    /// under the user's data directory (`%LOCALAPPDATA%` or `$XDG_DATA_HOME`, else `~/.local/share`).
    pub data_dir: Option<PathBuf>,
    /// Windows: whether the host refuses to start when another user's account owns or may change its
    /// configuration file or data directory, which would let that user decide who may connect. With
    /// `false` the host starts, logs what it found once, and the operator vouches for the directory.
    /// Elsewhere nothing is checked. Default `true`.
    pub check_access: bool,
    /// Whether the user's file is read over the system's when neither `--config` nor `BOHIO_CONFIG`
    /// names the files. Set in the lowest layer only. Default `true`.
    pub user_layer: bool,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            name: "bohio".to_owned(),
            data_dir: None,
            check_access: true,
            user_layer: true,
        }
    }
}

/// One `[[listen]]` entry: an endpoint and, for its scheme, the settings of that transport. A key for
/// another scheme is refused. A serial setting the endpoint's own query (`?baud=9600`) gives wins over
/// the key.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenConfig {
    /// `scheme://address`: `tcp://127.0.0.1:7620`, `pipe://bohio`, `pipe:///run/bohio.sock`,
    /// `ws://127.0.0.1:7621`, `wss://0.0.0.0:7622`, `serial:///dev/ttyUSB0`. Required.
    pub endpoint: String,
    /// `pipe`, Unix: the socket's mode as octal text with the `0o` prefix. Default `"0o600"`: the
    /// owner alone.
    #[serde(default)]
    pub socket_mode: Option<String>,
    /// `pipe`, Windows: whether clients on other hosts may connect through file sharing. Default
    /// `false`.
    #[serde(default)]
    pub remote_clients: Option<bool>,
    /// `ws`, `wss`, `webtransport`: the origins a browser may connect from; a connection that names another origin is
    /// refused. Default none: any origin, and a client that names none.
    #[serde(default)]
    pub origins: Option<Vec<String>>,
    /// `wss`, `webtransport`: the PEM file holding the host's certificate and the intermediates that
    /// follow it, leaf first. Required with `tls_key`, and by `wss`. A `webtransport` entry without
    /// one makes its own certificate (see `tls_valid_days`).
    #[serde(default)]
    pub tls_cert: Option<PathBuf>,
    /// `wss`, `webtransport`: the PEM file holding the private key of `tls_cert`. Read when the listener starts and
    /// never logged. Required with `tls_cert`.
    #[serde(default)]
    pub tls_key: Option<PathBuf>,
    /// `webtransport` without `tls_cert`: how many days the certificate the host makes is valid, not zero.
    /// A browser pinning a certificate by its hash accepts at most 14, so a longer one is logged once
    /// at start as a certificate a browser will not accept; other clients pin it whatever its validity.
    /// The host makes a new one halfway through and prints its hash. Default `13`.
    #[serde(default)]
    pub tls_valid_days: Option<u32>,
    /// `webtransport` without `tls_cert`: how far before now the certificate the host makes starts to be valid, so a client
    /// whose clock is behind the host's accepts it at once. `0s` starts it now. Default `1h`. A client further behind
    /// is refused until its clock reaches the start (see `bohio remote --clock-skew`).
    #[serde(default)]
    pub tls_backdate: Option<Span>,
    /// `serial`: bits per second. Not zero. Default `115200`.
    #[serde(default)]
    pub baud: Option<u32>,
    /// `serial`: data bits per character, 5 to 8. Default `8`; the link's framing needs 8.
    #[serde(default)]
    pub data_bits: Option<u8>,
    /// `serial`: `none`, `odd` or `even`. Default `none`.
    #[serde(default)]
    pub parity: Option<String>,
    /// `serial`: `1` or `2`. Default `1`.
    #[serde(default)]
    pub stop_bits: Option<u8>,
    /// `serial`: `none` or `hardware`. Default `none`.
    #[serde(default)]
    pub flow_control: Option<String>,
}

impl ListenConfig {
    /// The scheme of the endpoint, the text before `://`.
    pub fn scheme(&self) -> &str {
        self.endpoint
            .split_once("://")
            .map_or("", |(scheme, _)| scheme)
    }

    /// The serial settings the keys give, each read as the endpoint's query would be.
    ///
    /// # Errors
    ///
    /// The key and why its value is refused.
    pub fn serial_options(
        &self,
    ) -> Result<bohio_client::dial::SerialOptions, (&'static str, String)> {
        let mut options = bohio_client::dial::SerialOptions::default();
        let texts = [
            ("baud", self.baud.map(|v| v.to_string())),
            ("data_bits", self.data_bits.map(|v| v.to_string())),
            ("parity", self.parity.clone()),
            ("stop_bits", self.stop_bits.map(|v| v.to_string())),
            ("flow_control", self.flow_control.clone()),
        ];
        for (key, text) in texts {
            if let Some(text) = text {
                options.set(key, &text).map_err(|reason| (key, reason))?;
            }
        }
        Ok(options)
    }

    /// The pipe socket mode the key gives.
    ///
    /// # Errors
    ///
    /// Why the text is not octal with the `0o` prefix, at most `0o7777`.
    pub fn socket_mode_bits(&self) -> Result<Option<u32>, String> {
        let Some(text) = &self.socket_mode else {
            return Ok(None);
        };
        text.strip_prefix("0o")
            .and_then(|digits| u32::from_str_radix(digits, 8).ok())
            .filter(|bits| *bits <= 0o7777)
            .map(Some)
            .ok_or_else(|| format!("`{text}` is not an octal mode such as `0o660`"))
    }
}

/// `[link]`: bounds on a peer's link.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LinkConfig {
    /// How often a running stream's permission is decided again, so a revoked peer's streams
    /// end. A number of seconds or text with a unit (`500ms`, `5s`, `2m`, `1h`). Default `5s`.
    pub recheck: Span,
    /// The most streams one peer may have open at once. At least 1. Default `64`.
    pub max_streams: usize,
    /// The most links open at once across peers, counting those still proving themselves; a
    /// connection past it is closed unread. At least 1. Default `128`.
    pub max_connections: usize,
    /// The most connections from one source (an IPv4 address, or an IPv6 /64) still proving
    /// themselves at once; one past it is closed unread, so one source cannot hold every
    /// `max_connections` slot. Behind a proxy every peer shares its source. At least 1. Default `16`.
    pub max_unproven_per_source: usize,
    /// The most links one proven peer may have open at once; a link past it is closed after its
    /// proof, so one peer cannot hold every `max_connections` slot. At least 1. Default `16`.
    pub max_links_per_peer: usize,
    /// How long a peer has to prove itself. Same units as `recheck`; not zero. Default `10s`.
    pub proof_timeout: Span,
    /// Bytes a peer may send on one stream ahead of what the host has read from it (the stream's
    /// credit window): the most a sender gets ahead of a slow reader. At least 1. Default `1048576`.
    pub window: u32,
    /// The most bytes of one message in one frame (a longer message is sent in parts), so a long
    /// message cannot hold other streams up. At least 1. Default `65536`.
    pub max_frame: usize,
    /// The largest message on a stream: more than the largest `chunk_size` (a frame carries a tag
    /// byte besides the chunk). Default `1048576`.
    pub max_message: usize,
    /// Bytes a stream holds in each direction between the link and its engine; a message larger
    /// than this passes alone. At least 1. Default `262144`.
    pub queue_bytes: usize,
    /// Messages the socket's reader holds ahead of the link once a peer is proven (one before).
    /// At least 1. Default `8`.
    pub read_ahead: usize,
    /// Bytes the socket's writer may have queued. At least 1. Default `1048576`.
    pub queued_bytes: usize,
    /// How long a frame the socket takes nothing of ends the link, and how long a finished
    /// stream's queue is given to drain. Not zero. Default `10s`.
    pub send_limit: Span,
    /// How long a send waits for credit before it reports no progress and is tried again. Not
    /// zero. Default `250ms`.
    pub send_wait: Span,
    /// How often a stream's threads look at whether it was closed. Not zero. Default `50ms`.
    pub poll_interval: Span,
    /// How often the link asks its peer whether it is there, and what keeps a quiet link from a proxy's or a NAT's idle
    /// timeout; both sides ask. A number of seconds or text with a unit; `0` turns it off. Default `25s`.
    pub heartbeat: Span,
    /// Heartbeat intervals with nothing heard after which the link is lost: a peer that went away without closing
    /// is noticed in about `heartbeat` times this. At least 1. Default `3`.
    pub heartbeat_misses: u32,
}

impl Default for LinkConfig {
    fn default() -> Self {
        let link = bohio_client::link::Settings::default();
        Self {
            recheck: Span(Duration::from_secs(5)),
            max_streams: 64,
            max_connections: 128,
            max_unproven_per_source: 16,
            max_links_per_peer: 16,
            proof_timeout: Span(link.proof_timeout),
            window: link.window,
            max_frame: link.max_frame,
            max_message: link.max_message,
            queue_bytes: link.queue_bytes,
            read_ahead: link.read_ahead,
            queued_bytes: link.queued_bytes,
            send_limit: Span(link.send_limit),
            send_wait: Span(link.send_wait),
            poll_interval: Span(link.poll_interval),
            heartbeat: Span(link.heartbeat),
            heartbeat_misses: link.heartbeat_misses,
        }
    }
}

impl LinkConfig {
    /// The link bounds the host and a client share.
    pub fn settings(&self) -> bohio_client::link::Settings {
        let mut settings = bohio_client::link::Settings::default();
        settings.max_streams = self.max_streams;
        settings.proof_timeout = self.proof_timeout.0;
        settings.window = self.window;
        settings.max_frame = self.max_frame;
        settings.max_message = self.max_message;
        settings.queue_bytes = self.queue_bytes;
        settings.read_ahead = self.read_ahead;
        settings.queued_bytes = self.queued_bytes;
        settings.send_limit = self.send_limit.0;
        settings.send_wait = self.send_wait.0;
        settings.poll_interval = self.poll_interval.0;
        settings.heartbeat = self.heartbeat.0;
        settings.heartbeat_misses = self.heartbeat_misses;
        settings
    }
}

/// `[peer."<id>".dial]`: how this node dials a peer whose table names an `endpoint`, with a default for every
/// setting. Usable by an application that keeps its peers elsewhere: it deserializes from any table.
///
/// After a failure the node waits `retry_min`, then `retry_min x retry_factor`, and so on up to `retry_max`,
/// each wait changed by up to `retry_jitter` of itself in either direction so many nodes do not retry together;
/// a link that stayed up at least `retry_min` starts the sequence again. A smaller `retry_min` retries faster and
/// asks the peer, and a watching network, more often; a larger `retry_max` takes longer to notice a peer that
/// came back.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DialConfig {
    /// PEM files whose certificates are the trusted roots for a `wss://` or `webtransport://` endpoint, instead of
    /// the system's. Default none.
    pub ca: Vec<PathBuf>,
    /// SHA-256 hashes of certificates trusted as they are (`sha256:` and 64 hex digits) for a TLS endpoint. Default
    /// none.
    pub cert_hash: Vec<String>,
    /// For a `webtransport://` endpoint, a `ws://` or `wss://` endpoint of the same host tried when the QUIC path
    /// fails before anything crossed. Default none.
    pub fallback: Option<String>,
    /// How long opening the path may take (the socket, the WebSocket handshake, the program); the proof has
    /// `[link] proof_timeout` besides. Not zero. Default `10s`.
    pub timeout: Span,
    /// The first wait after a failure. Not zero. Default `1s`.
    pub retry_min: Span,
    /// The longest wait. At least `retry_min`. Default `60s`.
    pub retry_max: Span,
    /// What each further failure multiplies the wait by. At least 1 (1 is a fixed wait). Default `2`.
    pub retry_factor: f64,
    /// How far a wait may move, as a fraction of itself: 0 (none) to 1. Default `0.1`.
    pub retry_jitter: f64,
}

impl Default for DialConfig {
    fn default() -> Self {
        Self {
            ca: Vec::new(),
            cert_hash: Vec::new(),
            fallback: None,
            timeout: Span(Duration::from_secs(10)),
            retry_min: Span(Duration::from_secs(1)),
            retry_max: Span(Duration::from_secs(60)),
            retry_factor: 2.0,
            retry_jitter: 0.1,
        }
    }
}

impl DialConfig {
    /// Checks the settings: the key of a bad one (relative to `[peer."<id>".dial]`) and why.
    ///
    /// # Errors
    ///
    /// The key and the reason, for a value that means nothing.
    pub fn validate(&self) -> Result<(), (&'static str, &'static str)> {
        if self.timeout.0.is_zero() {
            return Err(("timeout", "must not be zero"));
        }
        if self.retry_min.0.is_zero() {
            return Err(("retry_min", "must not be zero"));
        }
        if self.retry_max.0 < self.retry_min.0 {
            return Err(("retry_max", "must be at least retry_min"));
        }
        if !self.retry_factor.is_finite() || self.retry_factor < 1.0 {
            return Err(("retry_factor", "must be at least 1"));
        }
        if !(0.0..=1.0).contains(&self.retry_jitter) {
            return Err(("retry_jitter", "must be from 0 to 1"));
        }
        Ok(())
    }
}

/// `[control]`: the local endpoint of a running node, through which `bohio remote --via` and `bohio links` reach
/// the links the node holds (including those a peer dialed). Whoever can connect there acts with everything
/// every peer grants this node, so it is guarded like the key file: by default this user alone.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ControlConfig {
    /// Whether the node serves the endpoint. Default `true`.
    pub enabled: Option<bool>,
    /// A `pipe://` endpoint. Default `pipe://bohio-` and the first 16 hex digits of the node's id, so two accounts'
    /// nodes never share a Windows pipe name.
    pub endpoint: Option<String>,
    /// Unix: the socket's mode as octal text with the `0o` prefix. Default `"0o600"`: the owner alone. Setting it
    /// (or `socket_group` or `security_descriptor`) makes the operator's choice the only check: with none set a
    /// client that is not this user is closed unread as well.
    pub socket_mode: Option<String>,
    /// Unix: the socket's group, a name or a numeric id. Default the process's own.
    pub socket_group: Option<String>,
    /// Windows: the pipe's security descriptor (SDDL). Default this user alone.
    pub security_descriptor: Option<String>,
}

impl ControlConfig {
    /// Whether the endpoint is served.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// The endpoint text: the one set, else `pipe://bohio-` and the first 16 hex digits of `node`'s digest.
    pub fn endpoint_for(&self, node: &coqui_peer::PeerId) -> String {
        self.endpoint.clone().unwrap_or_else(|| {
            let text = node.to_string();
            let digits = text.rsplit(':').next().unwrap_or("");
            format!("pipe://bohio-{}", &digits[..digits.len().min(16)])
        })
    }

    /// Whether the operator set who may connect, which then replaces the same-user check.
    pub fn sets_access(&self) -> bool {
        self.socket_mode.is_some()
            || self.socket_group.is_some()
            || self.security_descriptor.is_some()
    }

    /// The socket mode's bits.
    ///
    /// # Errors
    ///
    /// Why the text is not an octal mode with the `0o` prefix.
    pub fn socket_mode_bits(&self) -> Result<Option<u32>, String> {
        let Some(text) = &self.socket_mode else {
            return Ok(None);
        };
        text.strip_prefix("0o")
            .and_then(|digits| u32::from_str_radix(digits, 8).ok())
            .filter(|bits| *bits <= 0o7777)
            .map(Some)
            .ok_or_else(|| format!("`{text}` is not an octal mode such as `0o660`"))
    }
}

/// `[filesystem]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FilesystemConfig {
    /// Whether the host serves filesystem calls. Default `true`; a peer still needs `fs.read` or
    /// `fs.write` and a root.
    pub enabled: bool,
    /// The most file transfers (`bohio-read/1` and `bohio-write/1` streams) running at once,
    /// across peers. At least 1. Default `16`.
    pub max_transfers: usize,
    /// The most file bytes in one frame, and the size of the buffer a copy or a checksum reads
    /// with. At least 1; a smaller message bound of the stream wins. Default `65536`.
    pub chunk_size: usize,
    /// The most entries in one page of a directory listing, whatever the client asks for. At
    /// least 1. Default `1000`.
    pub list_page: usize,
    /// How long a transfer may make no progress (a client that stops reading or writing) before
    /// the host abandons it and frees its slot. Same units as `link.recheck`; not zero.
    /// Default `5m`.
    pub idle_timeout: Span,
    /// How often a transfer looks at its stream while it waits. Not zero. Default `20ms`.
    pub poll_interval: Span,
    /// Whether a write is flushed to storage before it is put in place. Default `true`.
    pub sync_writes: bool,
}

impl Default for FilesystemConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_transfers: 16,
            chunk_size: 64 * 1024,
            list_page: 1000,
            idle_timeout: Span(Duration::from_secs(300)),
            poll_interval: Span(Duration::from_millis(20)),
            sync_writes: true,
        }
    }
}

/// `[execution]`: one-shot processes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExecutionConfig {
    /// Whether the host runs processes. Default `true`; a peer still needs `exec`.
    pub enabled: bool,
    /// The most processes running at once, across peers. At least 1. Default `16`.
    pub max_processes: usize,
    /// The most output bytes in one frame. At least 1; a smaller bound of the stream wins.
    /// Default `16384`.
    pub chunk_size: usize,
    /// How often a running process, its deadline and its stream are looked at. Same units as
    /// `link.recheck`; not zero. Default `20ms`.
    pub poll_interval: Span,
    /// How long after a process exits its remaining output is awaited before the rest of its
    /// tree is ended. Default `500ms`.
    pub drain_timeout: Span,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_processes: 16,
            chunk_size: 16 * 1024,
            poll_interval: Span(Duration::from_millis(20)),
            drain_timeout: Span(Duration::from_millis(500)),
        }
    }
}

/// `[shell]`: interactive sessions.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ShellConfig {
    /// Whether the host opens shell sessions. Default `true`; a peer still needs `shell`.
    pub enabled: bool,
    /// The most sessions open at once, across peers. At least 1. Default `8`.
    pub max_sessions: usize,
    /// The most terminal bytes in one frame. At least 1; a smaller message bound of the stream
    /// wins. Default `16384`.
    pub chunk_size: usize,
    /// How often a session looks at its program and its stream while it waits. Same units as
    /// `link.recheck`; not zero. Default `20ms`.
    pub poll_interval: Span,
    /// How long the terminal must stay quiet after the program has exited before the session
    /// ends: the terminal can deliver the last of the output after the exit. Default `200ms`.
    pub drain_quiet: Span,
    /// How long the host waits for a session's output reader to finish after the program's tree
    /// has been ended, before it reports the exit regardless. Default `2s`.
    pub kill_wait: Span,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_sessions: 8,
            chunk_size: 16 * 1024,
            poll_interval: Span(Duration::from_millis(20)),
            drain_quiet: Span(Duration::from_millis(200)),
            kill_wait: Span(Duration::from_secs(2)),
        }
    }
}

/// A length of time, written as a number of seconds or as text with a unit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Span(pub Duration);

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl Span {
    /// Reads `30`, `250ms`, `5s`, `2m` or `1h` (a bare number is seconds).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let split = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (digits, unit) = text.split_at(split);
        let count: u64 = digits.parse().ok()?;
        let duration = match unit.trim() {
            "" | "s" => Duration::from_secs(count),
            "ms" => Duration::from_millis(count),
            "m" => Duration::from_secs(count.checked_mul(60)?),
            "h" => Duration::from_secs(count.checked_mul(3600)?),
            _ => return None,
        };
        Some(Self(duration))
    }
}

impl<'de> Deserialize<'de> for Span {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = Span;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number of seconds or text such as `250ms`, `5s`, `2m`, `1h`")
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Span, E> {
                Ok(Span(Duration::from_secs(value)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Span, E> {
                u64::try_from(value)
                    .map(|v| Span(Duration::from_secs(v)))
                    .map_err(|_| E::custom("a time cannot be negative"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Span, E> {
                Span::parse(value).ok_or_else(|| {
                    E::custom("not a time: use a number of seconds or a unit of ms, s, m or h")
                })
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// What a [`Node`](crate::node::Node) runs on: its name, the link bounds and the three engines'
/// settings. An embedding application reads it from any section of its own settings; `bohio.cfg`'s
/// `[service] name`, `[link]`, `[filesystem]`, `[execution]` and `[shell]` are one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NodeConfig {
    /// A name shown in `info` and logs. Default `bohio`.
    pub name: String,
    /// `[link]`.
    pub link: LinkConfig,
    /// `[filesystem]`.
    pub filesystem: FilesystemConfig,
    /// `[execution]`.
    pub execution: ExecutionConfig,
    /// `[shell]`.
    pub shell: ShellConfig,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: ServiceConfig::default().name,
            link: LinkConfig::default(),
            filesystem: FilesystemConfig::default(),
            execution: ExecutionConfig::default(),
            shell: ShellConfig::default(),
        }
    }
}

impl NodeConfig {
    /// Checks every bound: `label` names where the settings came from in the error.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] naming the key, for a value out of range.
    pub fn validate(&self, label: &str) -> Result<(), ConfigError> {
        let invalid = |key: &str, message: &str| ConfigError::Invalid {
            path: label.to_owned(),
            key: key.to_owned(),
            message: message.to_owned(),
        };
        if self.link.max_streams == 0 {
            return Err(invalid("link.max_streams", "must be at least 1"));
        }
        if self.link.recheck.0.is_zero() {
            return Err(invalid("link.recheck", "must not be zero"));
        }
        let link = &self.link;
        for (key, zero) in [
            ("link.max_connections", link.max_connections == 0),
            (
                "link.max_unproven_per_source",
                link.max_unproven_per_source == 0,
            ),
            ("link.max_links_per_peer", link.max_links_per_peer == 0),
            ("link.window", link.window == 0),
            ("link.max_message", link.max_message == 0),
            ("link.queue_bytes", link.queue_bytes == 0),
            ("link.read_ahead", link.read_ahead == 0),
            ("link.heartbeat_misses", link.heartbeat_misses == 0),
            ("link.queued_bytes", link.queued_bytes == 0),
        ] {
            if zero {
                return Err(invalid(key, "must be at least 1"));
            }
        }
        for (key, zero) in [
            ("link.proof_timeout", link.proof_timeout.0.is_zero()),
            ("link.send_limit", link.send_limit.0.is_zero()),
            ("link.send_wait", link.send_wait.0.is_zero()),
            ("link.poll_interval", link.poll_interval.0.is_zero()),
        ] {
            if zero {
                return Err(invalid(key, "must not be zero"));
            }
        }
        let largest_chunk = self
            .execution
            .chunk_size
            .max(self.filesystem.chunk_size)
            .max(self.shell.chunk_size);
        if link.max_message <= largest_chunk {
            return Err(invalid(
                "link.max_message",
                "must be more than the largest chunk_size",
            ));
        }
        if link.max_frame == 0 {
            return Err(invalid("link.max_frame", "must be at least 1"));
        }
        if self.execution.max_processes == 0 {
            return Err(invalid("execution.max_processes", "must be at least 1"));
        }
        if self.execution.chunk_size == 0 {
            return Err(invalid("execution.chunk_size", "must be at least 1"));
        }
        if self.execution.poll_interval.0.is_zero() {
            return Err(invalid("execution.poll_interval", "must not be zero"));
        }
        if self.filesystem.max_transfers == 0 {
            return Err(invalid("filesystem.max_transfers", "must be at least 1"));
        }
        if self.filesystem.chunk_size == 0 {
            return Err(invalid("filesystem.chunk_size", "must be at least 1"));
        }
        if self.filesystem.list_page == 0 {
            return Err(invalid("filesystem.list_page", "must be at least 1"));
        }
        if self.filesystem.idle_timeout.0.is_zero() {
            return Err(invalid("filesystem.idle_timeout", "must not be zero"));
        }
        if self.filesystem.poll_interval.0.is_zero() {
            return Err(invalid("filesystem.poll_interval", "must not be zero"));
        }
        if self.shell.max_sessions == 0 {
            return Err(invalid("shell.max_sessions", "must be at least 1"));
        }
        if self.shell.chunk_size == 0 {
            return Err(invalid("shell.chunk_size", "must be at least 1"));
        }
        if self.shell.poll_interval.0.is_zero() {
            return Err(invalid("shell.poll_interval", "must not be zero"));
        }
        Ok(())
    }
}

impl Config {
    /// Reads a configuration from one layer of TOML. `path` only labels errors.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Parse`] for bad TOML, an unknown key or a wrong type;
    /// [`ConfigError::Invalid`] for a value out of range.
    pub fn from_toml(text: &str, path: &str) -> Result<Self, ConfigError> {
        Layers::parse(text, path).map(|layered| layered.config)
    }

    /// Reads the configuration file at `path`.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Read`] when the file cannot be read, else as [`Config::from_toml`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Layers::load(&[path.to_owned()]).map(|layered| layered.config)
    }

    /// The data directory: the configured one, else the environment's default.
    pub fn data_dir(&self, environment: &Environment) -> PathBuf {
        self.service
            .data_dir
            .clone()
            .unwrap_or_else(|| environment.default_data_dir())
    }

    /// What a node runs on, from this file's `[service] name`, `[link]`, `[filesystem]`, `[execution]`
    /// and `[shell]`.
    pub fn node(&self) -> NodeConfig {
        NodeConfig {
            name: self.service.name.clone(),
            link: self.link.clone(),
            filesystem: self.filesystem.clone(),
            execution: self.execution.clone(),
            shell: self.shell.clone(),
        }
    }

    pub(crate) fn validate(&self, path: &str) -> Result<(), ConfigError> {
        let invalid = |key: &str, message: &str| ConfigError::Invalid {
            path: path.to_owned(),
            key: key.to_owned(),
            message: message.to_owned(),
        };
        self.node().validate(path)?;
        for listen in &self.listen {
            let shaped = listen
                .endpoint
                .split_once("://")
                .is_some_and(|(scheme, rest)| !scheme.is_empty() && !rest.is_empty());
            if !shaped {
                return Err(invalid(
                    "listen.endpoint",
                    "must look like `scheme://address`",
                ));
            }
            // A key belongs to one scheme: one set for another is a mistake worth naming.
            let given = [
                ("socket_mode", "pipe", listen.socket_mode.is_some()),
                ("remote_clients", "pipe", listen.remote_clients.is_some()),
                ("origins", "ws, wss, webtransport", listen.origins.is_some()),
                ("tls_cert", "wss, webtransport", listen.tls_cert.is_some()),
                ("tls_key", "wss, webtransport", listen.tls_key.is_some()),
                (
                    "tls_valid_days",
                    "webtransport",
                    listen.tls_valid_days.is_some(),
                ),
                (
                    "tls_backdate",
                    "webtransport",
                    listen.tls_backdate.is_some(),
                ),
                ("baud", "serial", listen.baud.is_some()),
                ("data_bits", "serial", listen.data_bits.is_some()),
                ("parity", "serial", listen.parity.is_some()),
                ("stop_bits", "serial", listen.stop_bits.is_some()),
                ("flow_control", "serial", listen.flow_control.is_some()),
            ];
            for (key, schemes, set) in given {
                if set && !schemes.split(", ").any(|scheme| scheme == listen.scheme()) {
                    return Err(invalid(
                        &format!("listen.{key}"),
                        &format!(
                            "is for `{}` endpoints, not `{}`",
                            schemes
                                .split(", ")
                                .map(|scheme| format!("{scheme}://"))
                                .collect::<Vec<_>>()
                                .join("` or `"),
                            listen.endpoint
                        ),
                    ));
                }
            }
            if listen.tls_backdate.is_some() && listen.tls_cert.is_some() {
                return Err(invalid(
                    "listen.tls_backdate",
                    "is for the certificate the host makes: drop it, or drop `tls_cert`",
                ));
            }
            if let Some(days) = listen.tls_valid_days {
                if days == 0 {
                    return Err(invalid("listen.tls_valid_days", "must be at least 1"));
                }
                if listen.tls_cert.is_some() {
                    return Err(invalid(
                        "listen.tls_valid_days",
                        "is for the certificate the host makes: drop it, or drop `tls_cert`",
                    ));
                }
            }
            match (&listen.tls_cert, &listen.tls_key) {
                (Some(_), None) => {
                    return Err(invalid("listen.tls_key", "is required with `tls_cert`"));
                }
                (None, Some(_)) => {
                    return Err(invalid("listen.tls_cert", "is required with `tls_key`"));
                }
                (None, None) if listen.scheme() == "wss" => {
                    return Err(invalid(
                        "listen.tls_cert",
                        &format!("`{}` needs `tls_cert` and `tls_key`", listen.endpoint),
                    ));
                }
                _ => {}
            }
            if let Err((key, reason)) = listen.serial_options() {
                return Err(invalid(&format!("listen.{key}"), &reason));
            }
            if let Err(reason) = listen.socket_mode_bits() {
                return Err(invalid("listen.socket_mode", &reason));
            }
        }
        if let Some(endpoint) = &self.control.endpoint
            && !endpoint.starts_with("pipe://")
        {
            return Err(invalid(
                "control.endpoint",
                "must be a `pipe://` endpoint: the control endpoint is local",
            ));
        }
        if let Err(reason) = self.control.socket_mode_bits() {
            return Err(invalid("control.socket_mode", &reason));
        }
        for (name, endpoint, dial) in self.peer.dials() {
            if let Err((key, reason)) = dial.validate() {
                return Err(invalid(&format!("peer.{name}.dial.{key}"), reason));
            }
            if let Err(reason) = check_dialed(endpoint, dial.fallback.as_deref()) {
                return Err(invalid(&format!("peer.{name}.endpoint"), &reason));
            }
            let mut trust = bohio_client::dial::ServerTrust::default();
            for hash in &dial.cert_hash {
                if let Err(reason) = trust.pin(hash) {
                    return Err(invalid(&format!("peer.{name}.dial.cert_hash"), &reason));
                }
            }
        }
        Ok(())
    }
}

/// Whether `endpoint` (and `fallback` after it) could be dialed, without resolving a name: a name is resolved at
/// each attempt, and a host that is offline when the file is read is not a mistake in it.
fn check_dialed(endpoint: &str, fallback: Option<&str>) -> Result<(), String> {
    use bohio_client::dial::Endpoint;
    let (scheme, rest) = endpoint.split_once("://").ok_or_else(|| {
        "must look like `scheme://address`, as an endpoint of `bohio remote` does".to_owned()
    })?;
    let parsed = if scheme == "tcp" {
        if rest.is_empty() {
            return Err("`tcp://` needs a host and a port".to_owned());
        }
        None
    } else {
        Some(Endpoint::parse(endpoint)?)
    };
    match (parsed, fallback) {
        (Some(parsed), Some(text)) => Endpoint::parse(text)
            .and_then(|fallback| parsed.with_fallback(fallback))
            .map(|_| ()),
        (None, Some(_)) => Err(
            "a fallback is a `ws://` or `wss://` endpoint that follows a `webtransport://` one"
                .to_owned(),
        ),
        _ => Ok(()),
    }
}

/// The 1-based line and column of byte `offset` in `text`.
pub(crate) fn position(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}
