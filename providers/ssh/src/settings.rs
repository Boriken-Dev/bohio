// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `[source.<name>]` of an ssh source, and what each setting means once read.

use std::path::PathBuf;
use std::time::Duration;

use bohio_api::guatiao::library::ProviderError;
use bohio_api::guatiao::{FromValue, Schema, Status, ToValue};

/// How the host's key is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostKeyCheck {
    /// The key must be the pinned one or the one `known_hosts` holds for the host.
    Strict,
    /// As `Strict`, except that a host with no entry is trusted at first contact and the key it presented then is the
    /// one every later connection of this source must present.
    Tofu,
    /// Any key is accepted.
    Off,
}

/// `[source.<name>]` of an ssh source: the host, how to log in, and the bounds of what is done to it. Every key but
/// `host` is optional. The defaults check everything; a weaker choice is the operator's and is logged as `non-default:`.
#[derive(ToValue, FromValue, Schema, Debug, Clone, PartialEq, Eq)]
#[map(crate = ::bohio_api::guatiao)]
pub struct SshSettings {
    /// The host's name or address.
    pub host: String,
    /// The port. 1 to 65535. Default `22`.
    #[map(default = 22)]
    pub port: u64,
    /// The account to log in as. Default the node's own account name (`USER`, `USERNAME`).
    pub user: Option<String>,
    /// The private key file to log in with (a leading `~/` is the account's home). Default the first of
    /// `~/.ssh/id_ed25519`, `id_ecdsa` and `id_rsa` that exists.
    pub identity: Option<String>,
    /// Log in with a key the ssh agent holds first (Pageant or the OpenSSH agent's named pipe on Windows, `SSH_AUTH_SOCK`
    /// elsewhere); a missing agent is not an error. Default `true`.
    #[map(default = true)]
    #[schema(advanced)]
    pub agent: bool,
    /// Where the account's password is kept (`file:PATH`, `credential:NAME`, `keyring:SERVICE/ACCOUNT`), never the
    /// password. Tried after the keys.
    #[schema(sensitive)]
    pub password: Option<String>,
    /// Where the passphrase of an encrypted key file is kept, as `password`.
    #[schema(sensitive)]
    pub passphrase: Option<String>,
    /// The host's key as `sha256:HEX` of its public key blob. A pinned key is the only one accepted whatever
    /// `host_key_check` says, except `off`.
    pub host_key: Option<String>,
    /// The `known_hosts` file `strict` and `tofu` read. Default `~/.ssh/known_hosts`.
    pub known_hosts: Option<String>,
    /// `strict` (the key is pinned or in `known_hosts`), `tofu` (a host with no entry is trusted at first contact) or
    /// `off` (any key). Default `strict`.
    #[map(default = String::from("strict"))]
    pub host_key_check: String,
    /// How long connecting and logging in may take (`15s`, `500ms`). Not zero. Default `15s`.
    #[map(default = String::from("15s"))]
    pub connect_timeout: String,
    /// How often an idle connection is probed (`30s`); `0s` sends none, and a dead connection is then found only when
    /// something is sent on it. Default `30s`.
    #[map(default = String::from("30s"))]
    pub keepalive: String,
    /// Unanswered probes before the connection is dropped. Not zero. Default `3`.
    #[map(default = 3)]
    pub keepalive_max: u64,
    /// The most session channels open at once on the connection, which the server's own `MaxSessions` (10 in OpenSSH)
    /// also bounds: a request past it waits `operation_timeout` and is then `busy`. At least 1. Default `8`.
    #[map(default = 8)]
    pub max_channels: u64,
    /// Whether a dropped connection is made again on the next use. Default `true`.
    #[map(default = true)]
    pub reconnect: bool,
    /// How long after a failed connect the source is reported unavailable without trying again (`5s`, `0s` tries at
    /// every use). Default `5s`.
    #[map(default = String::from("5s"))]
    pub reconnect_after: String,
    /// How long a request waits for a channel, the server's answer to opening one and a write to the server (`30s`).
    /// Not zero. Default `30s`.
    #[map(default = String::from("30s"))]
    pub operation_timeout: String,
    /// How long output and the end of a channel are awaited after a program has exited (`1s`): a descendant that
    /// outlives the program can keep the channel open. Default `1s`.
    #[map(default = String::from("1s"))]
    pub exit_grace: String,
    /// Chunks of output a program holds ahead of the reader (the reader waits when they are full). At least 1.
    /// Default `16`.
    #[map(default = 16)]
    pub queue_chunks: u64,
    /// Threads of the runtime that drives the connection. At least 1. Default `2`.
    #[map(default = 2)]
    pub worker_threads: u64,
    /// The SFTP requests of one file kept in flight at once: reads ahead and writes behind. Throughput over a slow link is
    /// this many requests per round trip, and the host holds this many packets of a file in memory. At least 1.
    /// Default `16`.
    #[map(default = 16)]
    pub sftp_in_flight: u64,
    /// The bytes of data one SFTP read or write request carries. The server's own limit (`limits@openssh.com`) caps it.
    /// At least 1. Default `32768`.
    #[map(default = 32_768)]
    pub sftp_packet: u64,
    /// The X display on the host to serve as the source's desktop (`:0`): the host's own X server, reached over a
    /// channel to its socket, with the cookie `xauth` lists for it. Default none: the source has no desktop.
    pub x11_display: Option<String>,
    /// The directory of the host's X sockets (`X<n>` in it). Default `/tmp/.X11-unix`.
    #[map(default = String::from("/tmp/.X11-unix"))]
    #[schema(advanced)]
    pub x11_socket_dir: String,
    /// The program that lists the host's X cookie (`xauth list DISPLAY`); a host without it is served without a cookie.
    /// Default `xauth`.
    #[map(default = String::from("xauth"))]
    #[schema(advanced)]
    pub xauth: String,
    /// Programs this source starts show their windows on the node's own desktop (the way `ssh -X` does): the host's X
    /// connections come back over the channel to the node's display. Needs `X11Forwarding yes` and an `xauth` on the
    /// host. Default `false`.
    #[map(default = false)]
    pub x11_forward: bool,
    /// Forward as a trusted client (the way `ssh -Y` does): a program on the host can read and drive everything on the
    /// node's display. Forwarding needs it: this build cannot ask the display for an untrusted authorization, so
    /// `x11_forward` without it is refused. Default `false`.
    #[map(default = false)]
    pub x11_trusted: bool,
    /// The node's own display that forwarded windows go to (`:0`). Default the node's `DISPLAY`.
    pub x11_local_display: Option<String>,
    /// The forwarded X connections open at once; one past it is refused. At least 1. Default `64`.
    #[map(default = 64)]
    pub x11_max_connections: u64,
}

/// A time: a number and `ms`, `s`, `m` or `h`; a bare number is seconds.
pub(crate) fn span(text: &str) -> Option<Duration> {
    let text = text.trim();
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    let (number, unit) = text.split_at(digits);
    let number: u64 = number.parse().ok()?;
    match unit.trim() {
        "ms" => Some(Duration::from_millis(number)),
        "" | "s" => Some(Duration::from_secs(number)),
        "m" => Some(Duration::from_secs(number.checked_mul(60)?)),
        "h" => Some(Duration::from_secs(number.checked_mul(3600)?)),
        _ => None,
    }
}

fn refuse(message: String) -> ProviderError {
    ProviderError::new(Status::GUATIAO_ERR_BAD_VALUE, &message)
}

/// The number of a local X display name (`:0`, `:12.0`).
pub(crate) fn display_number(key: &str, text: &str) -> Result<u32, ProviderError> {
    text.trim()
        .strip_prefix(':')
        .map(|rest| rest.split('.').next().unwrap_or(rest))
        .and_then(|number| number.parse().ok())
        .ok_or_else(|| {
            refuse(format!(
                "{key} is not a display of the machine itself (`:0`): `{text}`"
            ))
        })
}

/// The settings once read and checked: durations parsed, paths expanded.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub identity: Option<PathBuf>,
    pub agent: bool,
    pub password: Option<String>,
    pub passphrase: Option<String>,
    pub pin: Option<String>,
    pub known_hosts: PathBuf,
    pub check: HostKeyCheck,
    pub connect_timeout: Duration,
    pub keepalive: Option<Duration>,
    pub keepalive_max: usize,
    pub max_channels: usize,
    pub reconnect: bool,
    pub reconnect_after: Duration,
    pub operation_timeout: Duration,
    pub exit_grace: Duration,
    pub queue_chunks: usize,
    pub worker_threads: usize,
    pub sftp_in_flight: usize,
    pub sftp_packet: usize,
    pub x11_display: Option<u32>,
    pub x11_socket_dir: String,
    pub xauth: String,
    pub x11_forward: bool,
    pub x11_local_display: Option<String>,
    pub x11_max_connections: usize,
}

/// The account's home directory, from the environment.
pub(crate) fn home() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .find_map(|name| std::env::var_os(name).filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

/// `path` with a leading `~/` (or `~\`) replaced by the home directory.
pub(crate) fn expand(path: &str) -> Result<PathBuf, String> {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home()
            .map(|home| home.join(rest))
            .ok_or_else(|| format!("{path}: the account has no home directory")),
        None => Ok(PathBuf::from(path)),
    }
}

impl SshSettings {
    /// The settings of `host` with every default: for an embedder, and a base for a test to change.
    #[must_use]
    pub fn new(host: &str) -> Self {
        Self {
            host: host.to_owned(),
            port: 22,
            user: None,
            identity: None,
            agent: true,
            password: None,
            passphrase: None,
            host_key: None,
            known_hosts: None,
            host_key_check: "strict".to_owned(),
            connect_timeout: "15s".to_owned(),
            keepalive: "30s".to_owned(),
            keepalive_max: 3,
            max_channels: 8,
            reconnect: true,
            reconnect_after: "5s".to_owned(),
            operation_timeout: "30s".to_owned(),
            exit_grace: "1s".to_owned(),
            queue_chunks: 16,
            worker_threads: 2,
            sftp_in_flight: 16,
            sftp_packet: 32_768,
            x11_display: None,
            x11_socket_dir: "/tmp/.X11-unix".to_owned(),
            xauth: "xauth".to_owned(),
            x11_forward: false,
            x11_trusted: false,
            x11_local_display: None,
            x11_max_connections: 64,
        }
    }

    /// Checks every key and parses the values; nothing touches the network or a file.
    pub(crate) fn read(&self) -> Result<Settings, ProviderError> {
        if self.host.trim().is_empty() {
            return Err(refuse("host names no machine".to_owned()));
        }
        let port = u16::try_from(self.port)
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| refuse("port is not between 1 and 65535".to_owned()))?;
        let time = |key: &str, text: &str, zero: bool| -> Result<Duration, ProviderError> {
            let value = span(text).ok_or_else(|| {
                refuse(format!(
                    "{key} is not a time (a number and ms, s, m or h): `{text}`"
                ))
            })?;
            if value.is_zero() && !zero {
                return Err(refuse(format!(
                    "{key} is 0, which could never succeed (a longer time)"
                )));
            }
            Ok(value)
        };
        let count = |key: &str, value: u64| -> Result<usize, ProviderError> {
            if value == 0 {
                return Err(refuse(format!("{key} is 0 (at least 1)")));
            }
            Ok(usize::try_from(value).unwrap_or(usize::MAX))
        };
        let check = match self.host_key_check.as_str() {
            "strict" => HostKeyCheck::Strict,
            "tofu" => HostKeyCheck::Tofu,
            "off" => HostKeyCheck::Off,
            other => {
                return Err(refuse(format!(
                    "host_key_check is `{other}`, not strict, tofu or off"
                )));
            }
        };
        let pin = match &self.host_key {
            Some(text) => {
                let hex = text
                    .strip_prefix("sha256:")
                    .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
                    .ok_or_else(|| {
                        refuse("host_key is not `sha256:` and 64 hex digits".to_owned())
                    })?;
                Some(format!("sha256:{}", hex.to_ascii_lowercase()))
            }
            None => None,
        };
        let identity = self
            .identity
            .as_deref()
            .map(expand)
            .transpose()
            .map_err(refuse)?;
        let known_hosts = match self.known_hosts.as_deref() {
            Some(path) => expand(path).map_err(refuse)?,
            None => home().unwrap_or_default().join(".ssh").join("known_hosts"),
        };
        let user = self
            .user
            .clone()
            .or_else(|| {
                ["USER", "USERNAME"]
                    .into_iter()
                    .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
            })
            .ok_or_else(|| refuse("user is not set and the node has no account name".to_owned()))?;
        if self.x11_forward && !self.x11_trusted {
            return Err(refuse(
                "x11_forward needs x11_trusted = true: this build cannot ask the node's display for an untrusted authorization, so a forwarded program is a trusted one (see docs/security.md)"
                    .to_owned(),
            ));
        }
        let keepalive = time("keepalive", &self.keepalive, true)?;
        Ok(Settings {
            host: self.host.trim().to_owned(),
            port,
            user,
            identity,
            agent: self.agent,
            password: self.password.clone(),
            passphrase: self.passphrase.clone(),
            pin,
            known_hosts,
            check,
            connect_timeout: time("connect_timeout", &self.connect_timeout, false)?,
            keepalive: (!keepalive.is_zero()).then_some(keepalive),
            keepalive_max: count("keepalive_max", self.keepalive_max)?,
            max_channels: count("max_channels", self.max_channels)?,
            reconnect: self.reconnect,
            reconnect_after: time("reconnect_after", &self.reconnect_after, true)?,
            operation_timeout: time("operation_timeout", &self.operation_timeout, false)?,
            exit_grace: time("exit_grace", &self.exit_grace, true)?,
            queue_chunks: count("queue_chunks", self.queue_chunks)?,
            worker_threads: count("worker_threads", self.worker_threads)?,
            sftp_in_flight: count("sftp_in_flight", self.sftp_in_flight)?,
            sftp_packet: count("sftp_packet", self.sftp_packet)?,
            x11_display: self
                .x11_display
                .as_deref()
                .map(|text| display_number("x11_display", text))
                .transpose()?,
            x11_socket_dir: self.x11_socket_dir.trim_end_matches('/').to_owned(),
            xauth: self.xauth.clone(),
            x11_forward: self.x11_forward,
            x11_local_display: self.x11_local_display.clone(),
            x11_max_connections: count("x11_max_connections", self.x11_max_connections)?,
        })
    }

    /// The choices that are weaker or looser than the defaults, as the lines the node logs when it builds the source.
    pub(crate) fn non_default(&self) -> Vec<String> {
        let mut found = Vec::new();
        if self.host_key_check != "strict" {
            found.push(format!(
                "host_key_check = {}: the host's identity is not held to a pinned or known key",
                self.host_key_check
            ));
        }
        if span(&self.keepalive).is_some_and(|keepalive| keepalive.is_zero()) {
            found.push(
                "keepalive = 0s: a dead connection is found only when something is sent".to_owned(),
            );
        }
        if self.x11_forward {
            found.push(
                "x11_forward = true: programs on the host open windows on the node's own display"
                    .to_owned(),
            );
        }
        if self.x11_trusted {
            found.push(
                "x11_trusted = true: a forwarded program can read and drive the node's whole display"
                    .to_owned(),
            );
        }
        if self.x11_display.is_some() {
            found.push(
                "x11_display is set: a peer holding the desktop sees and types at the host's X display".to_owned(),
            );
        }
        if self.max_channels > 8 {
            found.push(format!(
                "max_channels = {}: above the 8 that stays below a server's default of 10",
                self.max_channels
            ));
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::span;
    use std::time::Duration;

    #[test]
    fn a_time_is_a_number_and_a_unit() {
        assert_eq!(span("15s"), Some(Duration::from_secs(15)));
        assert_eq!(span("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(span("2"), Some(Duration::from_secs(2)));
        assert_eq!(span("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(span("soon"), None);
    }
}

#[cfg(test)]
mod x11_tests {
    use super::{SshSettings, display_number};

    #[test]
    fn a_display_is_a_local_name() {
        assert_eq!(display_number("k", ":0").unwrap(), 0);
        assert_eq!(display_number("k", ":12.1").unwrap(), 12);
        assert!(display_number("k", "host:0").is_err());
        assert!(display_number("k", "0").is_err());
        assert!(display_number("k", ":x").is_err());
    }

    #[test]
    fn x_settings_default_off_and_forwarding_needs_trust() {
        let mut settings = SshSettings::new("example.invalid");
        let read = settings.read().unwrap();
        assert!(read.x11_display.is_none() && !read.x11_forward);
        assert!(settings.non_default().is_empty());

        settings.x11_forward = true;
        assert!(
            settings
                .read()
                .unwrap_err()
                .message()
                .contains("x11_trusted")
        );
        settings.x11_trusted = true;
        settings.x11_display = Some(":1".to_owned());
        assert_eq!(settings.read().unwrap().x11_display, Some(1));
        assert_eq!(settings.non_default().len(), 3);

        settings.x11_max_connections = 0;
        assert!(
            settings
                .read()
                .unwrap_err()
                .message()
                .contains("x11_max_connections")
        );
    }
}
