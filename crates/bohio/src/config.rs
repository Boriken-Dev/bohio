//! `bohio.cfg`: the host's configuration, read from TOML as layers.
//!
//! Every key is optional and has a documented default. A key this version does not know is an
//! error naming it, so a misspelled setting never looks like it worked. [`Layers`] reads the system
//! file and the user's as one configuration, each merged over the one before; a peer is a `[peer.<id>]`
//! table in it.

mod gui;
mod layers;

pub use gui::{ApprovalMode, BACKENDS, GuiConfig, Indicator};
pub use layers::{Layered, Layers};

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::{self, Deserializer};

use bohio_proto::compress::{Codec, Compression};
use bohio_proto::hash::Algorithm;

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
    /// `[hash]`.
    pub hash: HashConfig,
    /// `[delta]`.
    pub delta: DeltaConfig,
    /// `[compression]`.
    pub compression: CompressionConfig,
    /// `[execution]`.
    pub execution: ExecutionConfig,
    /// `[shell]`.
    pub shell: ShellConfig,
    /// `[forward]`.
    pub forward: ForwardConfig,
    /// `[gui]`.
    pub gui: GuiConfig,
    /// `[pairing]`: the PIN a node arms.
    pub pairing: PairingConfig,
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
    /// `webtransport`: the UDP payload size, in bytes, a connection starts from before it has measured the path. Default `1200`,
    /// the smallest QUIC allows (a lower value is refused). A larger one sends bigger packets from the start, which is
    /// efficient where the path carries them; where it does not they are dropped until black-hole detection falls to
    /// `mtu_min`, and the connection stalls meanwhile.
    #[serde(default)]
    pub mtu_initial: Option<u16>,
    /// `webtransport`: the size the path is known to carry, in bytes, to which Initial packets are padded and below which
    /// black-hole detection never goes. Default `1200`, the smallest QUIC allows (a lower value is refused). A value above
    /// what the path carries means no connection opens.
    #[serde(default)]
    pub mtu_min: Option<u16>,
    /// `webtransport`: whether the host searches for a larger size than `mtu_initial` (path-MTU discovery, RFC 8899).
    /// Default `true`. Off, the size stays at `mtu_initial`: no probe is lost, and a path that could carry more is used less
    /// fully (more packets per megabyte).
    #[serde(default)]
    pub mtu_discovery: Option<bool>,
    /// `webtransport`: the largest size discovery searches for, in bytes. Default `1452`, an Ethernet path; QUIC caps it at
    /// 65527. Each probe above the path's size is lost, and a host may log that as a send error, so set it to the path's
    /// size (a tunnel's MTU less 28) where the path is smaller. Higher finds a path of jumbo frames.
    #[serde(default)]
    pub mtu_upper_bound: Option<u16>,
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

    /// The UDP payload sizes and path-MTU discovery the keys give (quinn's defaults for a key not set).
    ///
    /// # Errors
    ///
    /// The key and why its value is refused: `mtu_initial` or `mtu_min` below the 1200 bytes QUIC requires, naming the value.
    pub fn web_mtu(&self) -> Result<bohio_client::dial::Mtu, (&'static str, String)> {
        let mut mtu = bohio_client::dial::Mtu::default();
        if let Some(initial) = self.mtu_initial {
            mtu.initial = initial;
        }
        if let Some(min) = self.mtu_min {
            mtu.min = min;
        }
        if let Some(discovery) = self.mtu_discovery {
            mtu.discovery = discovery;
        }
        if let Some(upper_bound) = self.mtu_upper_bound {
            mtu.upper_bound = upper_bound;
        }
        check_mtu(&mtu)?;
        Ok(mtu)
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
    /// How often the link itself asks whether the peer's key is still accepted, and ends the link (after telling each running
    /// stream it was revoked) when it is not. Same units as `recheck`; not zero. Unset: `2 x recheck`.
    pub key_recheck: Option<Span>,
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
    /// The most a stream's receive window may grow to, by `bulk_window` or `auto_window`: the memory one stream whose
    /// reader falls behind can cost. At least `window` and `bulk_window`. Unset: the larger of the two, so nothing is
    /// held beyond what was asked for.
    pub max_window: Option<u32>,
    /// Grows a stream's receive window by itself while data arrives (a `ping` per round trip), up to `max_window`, so a
    /// long path is filled without knowing its bandwidth-delay product; it never shrinks. Needs a `max_window` above
    /// `window` to do anything. Default `false`.
    pub auto_window: bool,
    /// The receive window this side grants a bulk stream it receives: a client's `bohio-read/1`, a host's
    /// `bohio-write/1`. One stream moves at most its window per round trip, so on a path with a round trip of 250 ms a
    /// 1 MiB window caps one transfer near 4 MB/s, and a larger value lifts it, at that much memory for each bulk stream
    /// whose reader falls behind. At most `max_window`; a value below `window` changes nothing, since a window is only ever
    /// widened. Unset: `window`.
    pub bulk_window: Option<u32>,
    /// The most bytes of one message in one frame (a longer message is sent in parts), so a long
    /// message cannot hold other streams up. At least 1. Default `65536`.
    pub max_frame: usize,
    /// The most metadata one open may carry; an open with more is refused. At least 1. Default `4096`.
    pub max_metadata: usize,
    /// Streams a peer may have opened that nothing has accepted yet. At least 1. Default `8`.
    pub max_pending_opens: usize,
    /// Streams a peer may open in a minute; ten a second by default, which caps a sync that opens one stream for each
    /// small file, so raise it for one. At least 1. Default `600`.
    pub max_opens_per_minute: u32,
    /// The TCP socket's send buffer in bytes, set before connecting and on accept; at least 1. Unset: the system's
    /// automatic sizing, which is the right choice unless a path's bandwidth-delay product exceeds what it reaches. The
    /// system may clamp it.
    pub tcp_send_buffer: Option<usize>,
    /// The TCP socket's receive buffer in bytes: see `tcp_send_buffer`. At least 1. Unset: the system's automatic sizing.
    pub tcp_receive_buffer: Option<usize>,
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
            key_recheck: None,
            max_streams: 64,
            max_connections: 128,
            max_unproven_per_source: 16,
            max_links_per_peer: 16,
            proof_timeout: Span(link.proof_timeout),
            window: link.window,
            max_window: None,
            auto_window: link.auto_window,
            bulk_window: None,
            max_frame: link.max_frame,
            max_metadata: link.max_metadata,
            max_pending_opens: link.max_pending_opens,
            max_opens_per_minute: link.max_opens_per_minute,
            tcp_send_buffer: link.tcp_send_buffer,
            tcp_receive_buffer: link.tcp_receive_buffer,
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
        settings.bulk_window = self.bulk_window.unwrap_or(self.window);
        settings.max_window = self
            .max_window
            .unwrap_or_else(|| self.window.max(settings.bulk_window));
        settings.auto_window = self.auto_window;
        settings.max_frame = self.max_frame;
        settings.max_metadata = self.max_metadata;
        settings.max_pending_opens = self.max_pending_opens;
        settings.max_opens_per_minute = self.max_opens_per_minute;
        settings.tcp_send_buffer = self.tcp_send_buffer;
        settings.tcp_receive_buffer = self.tcp_receive_buffer;
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
    /// For a `webtransport://` endpoint: the UDP payload size, in bytes, a connection starts from before it has measured the
    /// path. Default `1200`, the smallest QUIC allows (a lower value is refused). A larger one sends bigger packets from the
    /// start; where the path does not carry them they are dropped until black-hole detection falls to `mtu_min`, and the
    /// link stalls meanwhile.
    pub mtu_initial: u16,
    /// For a `webtransport://` endpoint: the size the path is known to carry, in bytes; Initial packets are padded to it and
    /// black-hole detection never goes below it. Default `1200`, the smallest QUIC allows (a lower value is refused). Above
    /// what the path carries, no connection opens.
    pub mtu_min: u16,
    /// For a `webtransport://` endpoint: whether the dialer searches for a larger size than `mtu_initial` (path-MTU
    /// discovery). Default `true`. Off, the size stays at `mtu_initial`: no probe is lost, and a path that could carry more is
    /// used less fully.
    pub mtu_discovery: bool,
    /// For a `webtransport://` endpoint: the largest size discovery searches for, in bytes. Default `1452`, an Ethernet
    /// path; QUIC caps it at 65527. Each probe above the path's size is lost and may be logged as a send error: on a
    /// tunnel set it to the tunnel's MTU less 28.
    pub mtu_upper_bound: u16,
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
            mtu_initial: bohio_client::dial::MIN_MTU,
            mtu_min: bohio_client::dial::MIN_MTU,
            mtu_discovery: true,
            mtu_upper_bound: bohio_client::dial::Mtu::default().upper_bound,
            timeout: Span(Duration::from_secs(10)),
            retry_min: Span(Duration::from_secs(1)),
            retry_max: Span(Duration::from_secs(60)),
            retry_factor: 2.0,
            retry_jitter: 0.1,
        }
    }
}

impl DialConfig {
    /// The waits between attempts: `retry_min`, `retry_max`, `retry_factor` and `retry_jitter`.
    pub fn backoff(&self) -> bohio_client::keep::Backoff {
        let mut backoff = bohio_client::keep::Backoff::default();
        backoff.min = self.retry_min.0;
        backoff.max = self.retry_max.0;
        backoff.factor = self.retry_factor;
        backoff.jitter = self.retry_jitter;
        backoff
    }

    /// The UDP payload sizes and path-MTU discovery the `mtu_` keys give.
    ///
    /// # Errors
    ///
    /// The key (relative to `[peer."<id>".dial]`) and why: `mtu_initial` or `mtu_min` below the 1200 bytes QUIC requires,
    /// naming the value.
    pub fn web_mtu(&self) -> Result<bohio_client::dial::Mtu, (&'static str, String)> {
        let mut mtu = bohio_client::dial::Mtu::default();
        mtu.initial = self.mtu_initial;
        mtu.min = self.mtu_min;
        mtu.discovery = self.mtu_discovery;
        mtu.upper_bound = self.mtu_upper_bound;
        check_mtu(&mtu)?;
        Ok(mtu)
    }

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
    /// The most file bytes in one frame, and the size of the buffer a copy reads with. At least 1; a smaller message bound of the stream wins. Default `65536`.
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
    /// The digests a write answers in `write-done` when its request names none. Each must be one of
    /// `[hash] algorithms`. An empty list answers none, which saves the CPU of hashing every byte
    /// written. Default `["sha256"]`.
    pub write_hashes: Vec<Algorithm>,
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
            write_hashes: vec![Algorithm::Sha256],
        }
    }
}

/// `[hash]`: digests of files, for `bohio-hash/1` and for the `hashes` and `expect` of reads and writes.
///
/// Hashing reads every byte it hashes, so a peer that may read can make the host spend its disk and
/// CPU: `max_streams` bounds how many hash streams run, `rate` how fast each one reads. The
/// algorithms are the host's to allow: the cryptographic ones cost more CPU than `xxh3` or `crc32c`,
/// and `md5` and `sha1` are there for comparing with sums that still use them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HashConfig {
    /// The most `bohio-hash/1` streams running at once, across peers; one past it is `busy`. At least
    /// 1. Default `4`.
    pub max_streams: usize,
    /// The most bytes a second one hash stream reads; `0` is no limit. A limit protects a disk another
    /// service uses, and makes a long hash take as long as the bytes divided by it. Default `0`.
    pub rate: u64,
    /// The size of the buffer a hash reads with, in bytes. At least 1. A `rate` below twenty times
    /// this reads in smaller pieces so the limit holds within a twentieth of a second. Default
    /// `1048576`.
    pub read_size: usize,
    /// The algorithms the host computes, for hash streams, reads and writes; an unknown name is an
    /// error. At least one. Default every algorithm this build has.
    pub algorithms: Vec<Algorithm>,
}

impl Default for HashConfig {
    fn default() -> Self {
        Self {
            max_streams: 4,
            rate: 0,
            read_size: 1024 * 1024,
            algorithms: bohio_transfer::hash::available(),
        }
    }
}

/// `[delta]`: block signatures and deltas, for a client that has an old copy of a file (pull) or a host that
/// has one (push).
///
/// A signature of a file is its blocks' hashes; the size of a block is chosen from the file's size between
/// `min_block` and `max_block`. A delta write reads the target and writes a new file beside it, so the
/// peer needs `fs.read` as well as `fs.write`. What a delta read costs the host is the signature it holds
/// (`max_signature_bytes`) and a window of one block; a write's is the temporary file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DeltaConfig {
    /// The smallest block a signature the host makes uses. At least 1; a small block finds more of what
    /// stayed and makes a larger signature. Default `2048`.
    pub min_block: u32,
    /// The largest block a signature uses, and the largest a peer may ask for; a block is held in
    /// memory twice per stream. At least `min_block`. Default `1048576`.
    pub max_block: u32,
    /// The most bytes of signature a delta read takes from the client (a weak value and a strong hash
    /// per block); one past it is `busy`. At least 1. Default `67108864` (64 MiB).
    pub max_signature_bytes: u64,
    /// The size of file from which a client's `auto` uses a delta instead of a whole transfer: a delta of a
    /// small file saves less than its round trips cost. Advertised in `info`; the client decides. Default
    /// `1048576`.
    pub min_size: u64,
    /// How many times the target's size plus the bytes a delta write received its output may be, checked once
    /// more than a mebibyte has been produced, so a peer cannot make a few frames write a disk full; past it
    /// the write ends with `bad_request`. A file that repeats a small base more often than this is refused, so a
    /// host that takes such files sets `0`, which turns the check off. Default `1024`.
    pub max_expansion: u32,
}

impl Default for DeltaConfig {
    fn default() -> Self {
        Self {
            min_block: 2048,
            max_block: 1024 * 1024,
            max_signature_bytes: 64 * 1024 * 1024,
            min_size: 1024 * 1024,
            max_expansion: 1024,
        }
    }
}

impl DeltaConfig {
    fn validate(&self) -> Result<(), (&'static str, &'static str)> {
        if self.min_block == 0 {
            return Err(("delta.min_block", "must be at least 1"));
        }
        if self.max_block < self.min_block {
            return Err(("delta.max_block", "must be at least min_block"));
        }
        if self.max_block > (1 << 30) {
            return Err(("delta.max_block", "must be at most 1073741824"));
        }
        if self.max_signature_bytes == 0 {
            return Err(("delta.max_signature_bytes", "must be at least 1"));
        }
        Ok(())
    }
}

/// `[compression]`: what a host compresses for a peer that asks, and what it accepts compressed.
///
/// A client names the compression of each stream (`read`, `write`, `exec` output and input) and the
/// host applies these caps and reports what it used. Compression costs the host CPU and memory, so
/// every cost is a setting: a weak host allows `lz4` alone and a low `max_level`, a host on a thin
/// link allows high zstd levels and long windows. The control and shell streams are never compressed.
/// Whatever a peer sends compressed is decoded within `window_log_max` and `max_expansion`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CompressionConfig {
    /// The algorithms a peer may ask for, among those this build has (`zstd`, `lz4`, `deflate`). A
    /// request for another is `unsupported`. Empty allows none. Default every algorithm this build has.
    pub allowed: Vec<Codec>,
    /// What a request for `auto` means. It must be one of `allowed`. Default `zstd` at level 3 (`lz4`
    /// in a build without zstd).
    pub default: Compression,
    /// The highest zstd level a request gets; a higher one is clamped and the header says so. Level 19
    /// costs tens of mebibytes and a fraction of a core's speed. Default `19`.
    pub max_level: i32,
    /// The highest deflate level, 0 to 9. Default `9`.
    pub deflate_max_level: i32,
    /// The largest zstd window, as a power of two: it caps what the host compresses with and what it
    /// accepts (a decoder holds about one window of memory per stream). A long window finds repeats
    /// further back and costs memory on both ends. 10 to 31. Default `23` (8 MiB).
    pub window_log_max: u32,
    /// zstd worker threads per stream; `0` compresses in the stream's own thread. Above `0` needs a
    /// build with the `zstdmt` feature. Default `0`.
    pub workers: u32,
    /// File extensions (without the dot, any case) a read never compresses, because the files are
    /// compressed already. Default archives, images, audio, video, documents and packages.
    pub skip_extensions: Vec<String>,
    /// How many bytes from the start of a file a read compresses as a trial; when the result is not
    /// smaller than `sample_ratio` the file is sent raw. `0` turns the trial off (each frame still goes
    /// raw when it does not shrink). Default `65536`.
    pub sample_bytes: usize,
    /// The compressed-to-raw ratio of the trial above which a file is sent raw: `0.9` sends a file
    /// raw unless compression saves a tenth. Above `0`. Default `0.9`.
    pub sample_ratio: f64,
    /// Globs of files never compressed, matched against the whole path and the file name: a file
    /// named secret and the same bytes in a stream may share a compression context with bytes a peer
    /// chose, so a host keeps key material out of it. Default key, certificate and password-store
    /// names.
    pub exclude: Vec<String>,
    /// How many times its compressed size what a peer sends may expand to, checked once more than a
    /// mebibyte has been produced; past it the write or the standard input ends with `bad_request`.
    /// A real file can beat any ratio (zeros compress about 1000 to 1 with deflate and far more with
    /// zstd), so a host that must take such files sets `0`, which turns the check off and leaves only
    /// the disk to stop a bomb. Default `1024`.
    pub max_expansion: u32,
    /// Where prepared reads keep the compressed files. Absent: the command uses `prepared` in its
    /// data directory and an embedding application has no prepared reads.
    pub prepare_dir: Option<PathBuf>,
    /// The most compressed bytes one prepared read may hold; a larger result is `busy`. At least 1.
    /// Default `1073741824` (1 GiB).
    pub prepare_max_bytes: u64,
    /// How long a prepared file is kept after it was made, for resuming. Not zero. Default `10m`.
    pub prepare_ttl: Span,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        let allowed = bohio_transfer::compress::available();
        let default = if allowed.contains(&Codec::Zstd) {
            Compression::new(Codec::Zstd).level(3)
        } else {
            Compression::new(Codec::Lz4)
        };
        let words = |list: &str| list.split_whitespace().map(str::to_owned).collect();
        Self {
            allowed,
            default,
            max_level: 19,
            deflate_max_level: 9,
            window_log_max: 23,
            workers: 0,
            skip_extensions: words(
                "zip gz tgz bz2 xz zst lz4 7z rar jpg jpeg png gif webp mp3 mp4 mkv mov avi webm pdf docx xlsx pptx jar apk whl iso",
            ),
            sample_bytes: 64 * 1024,
            sample_ratio: 0.9,
            exclude: words("*.key *.pem *.p12 *.pfx id_* .env *.kdbx"),
            max_expansion: 1024,
            prepare_dir: None,
            prepare_max_bytes: 1024 * 1024 * 1024,
            prepare_ttl: Span(Duration::from_secs(600)),
        }
    }
}

impl CompressionConfig {
    /// The caps the codecs run under.
    pub const fn caps(&self) -> bohio_transfer::compress::Caps {
        bohio_transfer::compress::Caps {
            max_level_zstd: self.max_level,
            max_level_deflate: self.deflate_max_level,
            window_log_max: self.window_log_max,
            workers: self.workers,
        }
    }

    fn validate(&self) -> Result<(), (&'static str, &'static str)> {
        let have = bohio_transfer::compress::available();
        if self.allowed.iter().any(|codec| !have.contains(codec)) {
            return Err((
                "compression.allowed",
                "names an algorithm this build does not have (zstd, lz4, deflate)",
            ));
        }
        // A host that allows nothing has no default to check.
        if !self.allowed.is_empty()
            && (!self.default.algorithm.compresses()
                || !self.allowed.contains(&self.default.algorithm))
        {
            return Err((
                "compression.default",
                "must be one of the allowed algorithms, never none or auto",
            ));
        }
        if !(10..=31).contains(&self.window_log_max) {
            return Err(("compression.window_log_max", "must be from 10 to 31"));
        }
        if self.deflate_max_level > 9 || self.deflate_max_level < 0 {
            return Err(("compression.deflate_max_level", "must be from 0 to 9"));
        }
        if self.max_level < 1 {
            return Err(("compression.max_level", "must be at least 1"));
        }
        if self.workers > 0 && !bohio_transfer::compress::MULTITHREAD {
            return Err((
                "compression.workers",
                "needs a build with the zstdmt feature",
            ));
        }
        if !(self.sample_ratio.is_finite() && self.sample_ratio > 0.0) {
            return Err(("compression.sample_ratio", "must be more than zero"));
        }
        if self.prepare_max_bytes == 0 {
            return Err(("compression.prepare_max_bytes", "must be at least 1"));
        }
        if self.prepare_ttl.0.is_zero() {
            return Err(("compression.prepare_ttl", "must not be zero"));
        }
        if self
            .exclude
            .iter()
            .any(|glob| globset::Glob::new(glob).is_err())
        {
            return Err(("compression.exclude", "holds a glob that cannot be read"));
        }
        Ok(())
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

/// `[forward]`: connections the host makes for a peer (`bohio-connect/1` and `tcp/1`), and the bounds of a forward's pump,
/// which `bohio remote ... forward` and `connect` read from the client's own file as well.
///
/// A peer is allowed targets by its grant (`forward.connect` and `[peer.<id>.forward] connect`), not here: this section only
/// bounds what a granted forward costs the host.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ForwardConfig {
    /// Whether the host makes connections for peers. Default `true`; a peer still needs `forward.connect` and a rule.
    pub enabled: bool,
    /// How long the host waits for a target to answer, for each address its name resolves to. Not zero. Default `10s`.
    pub connect_timeout: Span,
    /// The most forwarded connections open at once, across peers; one past it is `busy`. At least 1. Default `64`.
    pub max_connections: usize,
    /// The most forwarded connections one peer has open at once; one past it is `busy`. At least 1. Default `16`.
    pub max_connections_per_peer: usize,
    /// The most bytes of a connection sent as one message; the link's `max_message` must allow it. At least 1. Default `16384`.
    pub chunk_size: usize,
    /// How long a connection that has stopped reading may hold what a stream that ended in order sent before it is reset;
    /// also how long a write to a socket or pipe may take. Not zero. Default `30s`.
    pub stalled: Span,
    /// How soon each direction of a pump looks at its stream again while its connection has nothing for it, growing from
    /// `idle_min` to `idle_max` and starting over after traffic. A longer wait spares the CPU of an idle forward and slows
    /// its first byte. Not zero; `idle_max` at least `idle_min`. Default `1ms`.
    pub idle_min: Span,
    /// The longest that wait grows to. Default `10ms`.
    pub idle_max: Span,
    /// How often a pump looks at its stream and its connection for an end while it waits for a message. Not zero. Default `50ms`.
    pub watch: Span,
    /// How often a local listener (`bohio remote ... forward`) looks for a connection and for the link's end, and how often a
    /// listener the host binds for a peer looks for one and for the end of its stream: the first byte of a remote forward's
    /// connection waits at most this long. Not zero. Default `50ms`.
    pub listen_poll: Span,
    /// How long `bohio remote` waits for the host to answer a connect, which covers the host's `connect_timeout` and the
    /// link's round trip. Not zero. Default `30s`.
    pub answer_timeout: Span,
    /// The most connections one local listener of `bohio remote ... forward` serves at once; one past it is closed at once.
    /// At least 1. Default `64`.
    pub max_local_connections: usize,
    /// How long a connection that arrives at a local listener while the link to the host is down waits for it to come back
    /// (`[peer."<id>".forward] local`, and `bohio remote ... forward`, which reconnect), and how long a connect waits for the
    /// host's answer on a link that ended before it came. After it the connection is closed. `0` closes it at once. A longer
    /// wait hides a longer outage from a client that would retry itself, and holds its connection, a thread and a slot of
    /// `max_local_connections` meanwhile. Default `30s`.
    pub wait_for_link: Span,
    /// How long an open the host refused because it was over a limit (`[link] max_pending_opens`, `max_streams`,
    /// `max_per_protocol`, `max_opens_per_minute`) is tried again before the connection fails: a burst of connections through
    /// one forward opens more streams at once than the host holds pending. `0` never retries. Default `10s`.
    pub open_retry: Span,
    /// How long to wait between those attempts, each moved by up to a quarter of itself either way. Not zero. Default `100ms`.
    pub open_retry_interval: Span,
    /// The most listeners (`bohio-listen/1`) open at once, across peers; one past it is `busy`. At least 1. Default `16`.
    pub max_listeners: usize,
    /// The most listeners one peer has open at once; one past it is `busy`. At least 1. Default `8`.
    pub max_listeners_per_peer: usize,
    /// The most arrivals one listener holds waiting for their `bohio-accept/1`; a connection past it is closed at once. A
    /// peer's `backlog` may lower it for its own listener. At least 1. Default `16`.
    pub max_pending: usize,
    /// How long an arrival waits to be claimed before the host closes it. Not zero. Default `10s`.
    pub accept_timeout: Span,
    /// The mode of a Unix socket a listener creates: this account alone by default. A wider mode is the operator's choice
    /// and is listed as `non-default` in the log; `ipc_same_user` still decides who is served. Default `"0o600"`.
    pub socket_mode: Mode,
    /// Whether a socket or pipe listener closes, unread, a connection from another account (as the operating system names
    /// it: the peer's user id on Unix, its SID on Windows). `false` serves any account that can reach the socket or pipe,
    /// which the mode, or on Windows the pipe's access list, would then be the only guard of. Default `true`.
    pub ipc_same_user: bool,
}

impl Default for ForwardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            connect_timeout: Span(Duration::from_secs(10)),
            max_connections: 64,
            max_connections_per_peer: 16,
            chunk_size: 16 * 1024,
            stalled: Span(Duration::from_secs(30)),
            idle_min: Span(Duration::from_millis(1)),
            idle_max: Span(Duration::from_millis(10)),
            watch: Span(Duration::from_millis(50)),
            listen_poll: Span(Duration::from_millis(50)),
            answer_timeout: Span(Duration::from_secs(30)),
            max_local_connections: 64,
            wait_for_link: Span(Duration::from_secs(30)),
            open_retry: Span(Duration::from_secs(10)),
            open_retry_interval: Span(Duration::from_millis(100)),
            max_listeners: 16,
            max_listeners_per_peer: 8,
            max_pending: 16,
            accept_timeout: Span(Duration::from_secs(10)),
            socket_mode: Mode(0o600),
            ipc_same_user: true,
        }
    }
}

impl ForwardConfig {
    /// The pump's bounds, as `coqui-link` names them.
    pub fn options(&self) -> coqui_link::forward::Options {
        let mut options = coqui_link::forward::Options::default();
        options.connect_timeout = self.connect_timeout.0;
        options.chunk = self.chunk_size;
        options.stalled = self.stalled.0;
        options.idle_min = self.idle_min.0;
        options.idle_max = self.idle_max.0;
        options.watch = self.watch.0;
        options.listen_poll = self.listen_poll.0;
        options
    }

    /// What a client waits for and how its pump moves bytes.
    pub fn client(&self) -> bohio_client::forward::ForwardSettings {
        let mut settings = bohio_client::forward::ForwardSettings::default();
        settings.options = self.options();
        settings.answer_timeout = self.answer_timeout.0;
        settings.max_connections = self.max_local_connections;
        settings.wait_for_link = self.wait_for_link.0;
        settings.open_retry = self.open_retry.0;
        settings.open_retry_interval = self.open_retry_interval.0;
        settings
    }
}

/// `[pairing]`: the PIN a node arms for a joiner to try on its own listeners.
///
/// A PIN has `10^pin_length` values with the default digits, and a guess is made only against a PIN armed now: with `max_attempts`
/// wrong guesses allowed, the chance that a stranger who reaches the node guesses one PIN is `max_attempts / 10^pin_length`
/// (`10 / 10^6`, one in a hundred thousand, by default). `attempt_delay` slows a source that guesses, so `max_attempts = 0`
/// (no limit) is bounded by time instead: from one source, the delay doubles from `attempt_delay` to `attempt_delay_max`, which is
/// about 15 guesses in the default five minutes, but every source has its own delay, so a stranger with many addresses
/// multiplies that. Unlimited trades the guess count for the lifetime, and is for a node that only a trusted network reaches.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PairingConfig {
    /// How long a PIN lasts when the request names no time. Same units as `[link] recheck`; not zero. Default `5m`.
    pub timeout: Span,
    /// Wrong guesses a PIN takes before it is used up; the last allowed wrong guess uses it up. `0` is no limit: the PIN then
    /// lasts until `timeout`. Default `10`.
    pub max_attempts: u32,
    /// How long a source must wait after a wrong guess before it may try again; the wait doubles at each further wrong guess
    /// from that source, up to `attempt_delay_max`. A source that tries sooner is refused without a try. `0` is no wait.
    /// Default `1s`.
    pub attempt_delay: Span,
    /// The longest wait `attempt_delay` grows to; at least `attempt_delay`. Default `30s`.
    pub attempt_delay_max: Span,
    /// How many digits a PIN has. At least 1. Default `6`.
    pub pin_length: usize,
}

impl Default for PairingConfig {
    fn default() -> Self {
        Self {
            timeout: Span(Duration::from_secs(300)),
            max_attempts: 10,
            attempt_delay: Span(Duration::from_secs(1)),
            attempt_delay_max: Span(Duration::from_secs(30)),
            pin_length: 6,
        }
    }
}

impl PairingConfig {
    fn validate(&self) -> Result<(), (&'static str, &'static str)> {
        if self.timeout.0.is_zero() {
            return Err(("pairing.timeout", "must not be zero"));
        }
        if self.attempt_delay_max.0 < self.attempt_delay.0 {
            return Err((
                "pairing.attempt_delay_max",
                "must be at least attempt_delay",
            ));
        }
        if self.pin_length == 0 {
            return Err(("pairing.pin_length", "must be at least 1"));
        }
        Ok(())
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

/// Permission bits for a Unix socket, written in octal with a `0o` prefix (`"0o600"`) or as a TOML octal integer (`0o600`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Mode(pub u32);

impl fmt::Debug for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0o{:o}", self.0)
    }
}

impl<'de> Deserialize<'de> for Mode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = Mode;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("permission bits such as `\"0o600\"` or `0o660`")
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Mode, E> {
                u32::try_from(value)
                    .ok()
                    .filter(|bits| *bits <= 0o7777)
                    .map(Mode)
                    .ok_or_else(|| E::custom("permission bits are at most 0o7777"))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Mode, E> {
                u64::try_from(value)
                    .map_err(|_| E::custom("permission bits cannot be negative"))
                    .and_then(|bits| self.visit_u64(bits))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Mode, E> {
                value
                    .strip_prefix("0o")
                    .and_then(|digits| u32::from_str_radix(digits, 8).ok())
                    .filter(|bits| *bits <= 0o7777)
                    .map(Mode)
                    .ok_or_else(|| E::custom("not permission bits: write them as `\"0o600\"`"))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// What a [`Node`](crate::node::Node) runs on: its name, the link bounds and the three engines'
/// settings. An embedding application reads it from any section of its own settings; `bohio.cfg`'s
/// `[service] name`, `[link]`, `[filesystem]`, `[execution]` and `[shell]` are one.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NodeConfig {
    /// A name shown in `info` and logs. Default `bohio`.
    pub name: String,
    /// `[link]`.
    pub link: LinkConfig,
    /// `[filesystem]`.
    pub filesystem: FilesystemConfig,
    /// `[hash]`.
    pub hash: HashConfig,
    /// `[delta]`.
    pub delta: DeltaConfig,
    /// `[compression]`.
    pub compression: CompressionConfig,
    /// `[execution]`.
    pub execution: ExecutionConfig,
    /// `[shell]`.
    pub shell: ShellConfig,
    /// `[forward]`.
    pub forward: ForwardConfig,
    /// `[gui]`.
    pub gui: GuiConfig,
    /// `[pairing]`.
    pub pairing: PairingConfig,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: ServiceConfig::default().name,
            link: LinkConfig::default(),
            filesystem: FilesystemConfig::default(),
            hash: HashConfig::default(),
            delta: DeltaConfig::default(),
            compression: CompressionConfig::default(),
            execution: ExecutionConfig::default(),
            shell: ShellConfig::default(),
            forward: ForwardConfig::default(),
            gui: GuiConfig::default(),
            pairing: PairingConfig::default(),
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
        if let Err((key, message)) = self.pairing.validate() {
            return Err(invalid(key, message));
        }
        if self.link.max_streams == 0 {
            return Err(invalid("link.max_streams", "must be at least 1"));
        }
        if self.link.key_recheck.is_some_and(|every| every.0.is_zero()) {
            return Err(invalid("link.key_recheck", "must not be zero"));
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
            ("link.max_window", link.max_window == Some(0)),
            ("link.bulk_window", link.bulk_window == Some(0)),
            ("link.max_metadata", link.max_metadata == 0),
            ("link.max_pending_opens", link.max_pending_opens == 0),
            ("link.max_opens_per_minute", link.max_opens_per_minute == 0),
            ("link.tcp_send_buffer", link.tcp_send_buffer == Some(0)),
            (
                "link.tcp_receive_buffer",
                link.tcp_receive_buffer == Some(0),
            ),
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
        if let Some(max) = link.max_window {
            if link.window > max {
                return Err(invalid("link.window", "must be at most link.max_window"));
            }
            if link.bulk_window.is_some_and(|bulk| bulk > max) {
                return Err(invalid(
                    "link.bulk_window",
                    "must be at most link.max_window",
                ));
            }
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
        if self.hash.max_streams == 0 {
            return Err(invalid("hash.max_streams", "must be at least 1"));
        }
        if self.hash.read_size == 0 {
            return Err(invalid("hash.read_size", "must be at least 1"));
        }
        if self.hash.algorithms.is_empty() {
            return Err(invalid(
                "hash.algorithms",
                "must name at least one algorithm",
            ));
        }
        let have = bohio_transfer::hash::available();
        if self.hash.algorithms.iter().any(|name| !have.contains(name)) {
            return Err(invalid(
                "hash.algorithms",
                "names an algorithm this build does not compute",
            ));
        }
        if self
            .filesystem
            .write_hashes
            .iter()
            .any(|name| !self.hash.algorithms.contains(name))
        {
            return Err(invalid(
                "filesystem.write_hashes",
                "must name only algorithms that [hash] algorithms allows",
            ));
        }
        if let Err((key, message)) = self.compression.validate() {
            return Err(invalid(key, message));
        }
        if let Err((key, message)) = self.delta.validate() {
            return Err(invalid(key, message));
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
        let forward = &self.forward;
        for (key, zero) in [
            ("forward.max_connections", forward.max_connections == 0),
            (
                "forward.max_connections_per_peer",
                forward.max_connections_per_peer == 0,
            ),
            ("forward.chunk_size", forward.chunk_size == 0),
            (
                "forward.max_local_connections",
                forward.max_local_connections == 0,
            ),
            ("forward.max_listeners", forward.max_listeners == 0),
            (
                "forward.max_listeners_per_peer",
                forward.max_listeners_per_peer == 0,
            ),
            ("forward.max_pending", forward.max_pending == 0),
        ] {
            if zero {
                return Err(invalid(key, "must be at least 1"));
            }
        }
        for (key, zero) in [
            (
                "forward.connect_timeout",
                forward.connect_timeout.0.is_zero(),
            ),
            ("forward.stalled", forward.stalled.0.is_zero()),
            ("forward.idle_min", forward.idle_min.0.is_zero()),
            ("forward.watch", forward.watch.0.is_zero()),
            ("forward.listen_poll", forward.listen_poll.0.is_zero()),
            ("forward.answer_timeout", forward.answer_timeout.0.is_zero()),
            ("forward.accept_timeout", forward.accept_timeout.0.is_zero()),
            (
                "forward.open_retry_interval",
                forward.open_retry_interval.0.is_zero(),
            ),
        ] {
            if zero {
                return Err(invalid(key, "must not be zero"));
            }
        }
        if forward.idle_max.0 < forward.idle_min.0 {
            return Err(invalid(
                "forward.idle_max",
                "must be at least forward.idle_min",
            ));
        }
        if let Err((key, message)) = self.gui.validate() {
            return Err(invalid(key, message));
        }
        if link.max_message <= self.gui.chunk_size {
            return Err(invalid(
                "link.max_message",
                "must be more than gui.chunk_size",
            ));
        }
        if link.max_message <= forward.chunk_size {
            return Err(invalid(
                "link.max_message",
                "must be more than forward.chunk_size",
            ));
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
            hash: self.hash.clone(),
            delta: self.delta.clone(),
            compression: self.compression.clone(),
            execution: self.execution.clone(),
            shell: self.shell.clone(),
            forward: self.forward.clone(),
            gui: self.gui.clone(),
            pairing: self.pairing.clone(),
        }
    }

    pub(crate) fn validate(&self, path: &str) -> Result<(), ConfigError> {
        let invalid = |key: &str, message: &str| ConfigError::Invalid {
            path: path.to_owned(),
            key: key.to_owned(),
            message: message.to_owned(),
        };
        self.node().validate(path)?;
        for id in self.peer.ids() {
            for rule in self.peer.grant_for(id).forward().connect() {
                if let Err(error) = rule.parse::<crate::forward::ConnectRule>() {
                    return Err(invalid(
                        &format!("peer.{id}.forward.connect"),
                        &format!("`{rule}`: {error}"),
                    ));
                }
            }
            for rule in self.peer.grant_for(id).forward().listen() {
                if let Err(error) = rule.parse::<crate::forward::ListenRule>() {
                    return Err(invalid(
                        &format!("peer.{id}.forward.listen"),
                        &format!("`{rule}`: {error}"),
                    ));
                }
            }
        }
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
                ("mtu_initial", "webtransport", listen.mtu_initial.is_some()),
                ("mtu_min", "webtransport", listen.mtu_min.is_some()),
                (
                    "mtu_discovery",
                    "webtransport",
                    listen.mtu_discovery.is_some(),
                ),
                (
                    "mtu_upper_bound",
                    "webtransport",
                    listen.mtu_upper_bound.is_some(),
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
            if let Err((key, reason)) = listen.web_mtu() {
                return Err(invalid(&format!("listen.{key}"), &reason));
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
        for (name, table) in self.peer.declared() {
            let key = format!("peer.{name}.forward");
            if name.parse::<coqui_peer::PeerId>().is_err() {
                return Err(invalid(
                    &key,
                    "`local` and `remote` need the peer's id as the table's name",
                ));
            }
            for (list, entries) in [
                ("local", table.forward_local()),
                ("remote", table.forward_remote()),
            ] {
                for entry in entries {
                    let parsed = if list == "local" {
                        entry.parse::<crate::forward::LocalSpec>().map(|_| ())
                    } else {
                        entry.parse::<crate::forward::RemoteSpec>().map(|_| ())
                    };
                    if let Err(reason) = parsed {
                        return Err(invalid(
                            &format!("{key}.{list}"),
                            &format!("`{entry}`: {reason}"),
                        ));
                    }
                }
            }
        }
        for (name, endpoint, dial) in self.peer.dials() {
            if let Err((key, reason)) = dial.validate() {
                return Err(invalid(&format!("peer.{name}.dial.{key}"), reason));
            }
            if let Err((key, reason)) = dial.web_mtu() {
                return Err(invalid(&format!("peer.{name}.dial.{key}"), &reason));
            }
            if dial
                .web_mtu()
                .is_ok_and(|mtu| mtu != bohio_client::dial::Mtu::default())
                && !endpoint.starts_with("webtransport://")
            {
                return Err(invalid(
                    &format!("peer.{name}.dial"),
                    "the `mtu_` keys are for a `webtransport://` endpoint",
                ));
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

/// Whether `mtu` can be used: the `mtu_` key that is too small, and why, naming its value.
fn check_mtu(mtu: &bohio_client::dial::Mtu) -> Result<(), (&'static str, String)> {
    mtu.check().map_err(|error| {
        (
            if error.key() == "initial" {
                "mtu_initial"
            } else {
                "mtu_min"
            },
            format!(
                "is {}, below the {} bytes QUIC requires",
                error.value(),
                bohio_client::dial::MIN_MTU
            ),
        )
    })
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
