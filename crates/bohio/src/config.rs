//! `bohio.cfg`: the host's configuration, found by a fixed search order and read from TOML.
//!
//! Every key is optional and has a documented default. A key this version does not know is an
//! error naming it, so a misspelled setting never looks like it worked.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::{self, Deserializer};

/// The environment a search for the configuration and the data directory reads, captured by the
/// binary so the library reads none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    /// Whether to use Windows locations (`%APPDATA%`, `%ProgramData%`) instead of Unix ones.
    pub windows: bool,
    /// `BOHIO_CONFIG`: a configuration file named outright.
    pub config: Option<PathBuf>,
    /// `APPDATA` (Windows): the user's configuration directory.
    pub app_data: Option<PathBuf>,
    /// `LOCALAPPDATA` (Windows): the user's data directory.
    pub local_app_data: Option<PathBuf>,
    /// `ProgramData` (Windows): the system's configuration directory.
    pub program_data: Option<PathBuf>,
    /// `XDG_CONFIG_HOME` (Unix).
    pub xdg_config_home: Option<PathBuf>,
    /// `XDG_DATA_HOME` (Unix).
    pub xdg_data_home: Option<PathBuf>,
    /// `HOME` (Unix).
    pub home: Option<PathBuf>,
    /// `PATH`: where programs and shells are looked for.
    pub path: Option<OsString>,
}

impl Environment {
    /// The configuration files to try, in order: `BOHIO_CONFIG`, then the user's, then the
    /// system's. Only the first is an error when it is missing.
    pub fn config_candidates(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        if let Some(path) = &self.config {
            found.push(path.clone());
        }
        if self.windows {
            found.extend(
                self.app_data
                    .iter()
                    .map(|d| d.join("bohio").join("bohio.cfg")),
            );
            found.extend(
                self.program_data
                    .iter()
                    .map(|d| d.join("bohio").join("bohio.cfg")),
            );
        } else {
            let user = self
                .xdg_config_home
                .clone()
                .or_else(|| self.home.as_ref().map(|h| h.join(".config")));
            found.extend(user.map(|d| d.join("bohio").join("bohio.cfg")));
            found.push(PathBuf::from("/etc/bohio/bohio.cfg"));
        }
        found
    }

    /// The directory holding the host's identity, trust store and peer policy when the
    /// configuration names none: under the user's data directory.
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
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file could not be read.
    #[error("{path}: cannot be read: {reason}")]
    Read {
        /// The file.
        path: String,
        /// The operating system's reason.
        reason: String,
    },
    /// The file is not valid TOML, or holds a key this version does not know, or a wrong type.
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
    /// A value is out of range.
    #[error("{path}: `{key}` {message}")]
    Invalid {
        /// The file, or `<text>`.
        path: String,
        /// The key, as written in the file.
        key: String,
        /// What is wrong with it.
        message: String,
    },
}

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
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
}

/// `[service]`: who the host is.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServiceConfig {
    /// A name shown in `info` and logs. Default `bohio`.
    pub name: String,
    /// The directory holding the identity, the trust store and `peers.cfg`. Default: `bohio`
    /// under the user's data directory (`%LOCALAPPDATA%` or `$XDG_DATA_HOME`, else `~/.local/share`).
    pub data_dir: Option<PathBuf>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            name: "bohio".to_owned(),
            data_dir: None,
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
        settings
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

impl Config {
    /// Reads a configuration from TOML. `path` only labels errors.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Parse`] for bad TOML, an unknown key or a wrong type;
    /// [`ConfigError::Invalid`] for a value out of range.
    pub fn from_toml(text: &str, path: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|error| {
            let (line, column) = error
                .span()
                .map_or((1, 1), |span| position(text, span.start));
            ConfigError::Parse {
                path: path.to_owned(),
                line,
                column,
                message: error.message().to_owned(),
            }
        })?;
        config.validate(path)?;
        Ok(config)
    }

    /// Reads the configuration file at `path`.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Read`] when the file cannot be read, else as [`Config::from_toml`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let label = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|error| ConfigError::Read {
            path: label.clone(),
            reason: error.kind().to_string(),
        })?;
        Self::from_toml(&text, &label)
    }

    /// The first configuration file `environment` names that exists, or the defaults when none
    /// does. `explicit` (a `--config` argument) is used alone and must exist.
    ///
    /// Returns the configuration and the file it came from.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Read`] when `explicit` or `BOHIO_CONFIG` names a file that cannot be read,
    /// else as [`Config::from_toml`].
    pub fn locate(
        explicit: Option<&Path>,
        environment: &Environment,
    ) -> Result<(Self, Option<PathBuf>), ConfigError> {
        if let Some(path) = explicit {
            return Self::load(path).map(|config| (config, Some(path.to_owned())));
        }
        for (index, candidate) in environment.config_candidates().into_iter().enumerate() {
            let named = index == 0 && environment.config.is_some();
            if named || candidate.is_file() {
                return Self::load(&candidate).map(|config| (config, Some(candidate)));
            }
        }
        Ok((Self::default(), None))
    }

    /// The data directory: the configured one, else the environment's default.
    pub fn data_dir(&self, environment: &Environment) -> PathBuf {
        self.service
            .data_dir
            .clone()
            .unwrap_or_else(|| environment.default_data_dir())
    }

    fn validate(&self, path: &str) -> Result<(), ConfigError> {
        let invalid = |key: &str, message: &str| ConfigError::Invalid {
            path: path.to_owned(),
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
        Ok(())
    }
}

/// The 1-based line and column of byte `offset` in `text`.
pub(crate) fn position(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}
