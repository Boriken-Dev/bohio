//! Forwards as text: the `LISTEN=TARGET` of `-L` and `-R` and of `[peer."<id>".forward] local` and `remote`.
//!
//! A local entry listens on this machine and has the host connect to its target: `[BIND:]PORT=TARGET`, `unix:PATH=TARGET` or
//! `pipe:NAME=TARGET`. A remote entry has the host listen and brings each connection to a target on this machine:
//! `[BIND:]PORT=LOCAL`, `unix:PATH=LOCAL` or `pipe:NAME=LOCAL`. A target is `HOST:PORT` (an IPv6 address in brackets),
//! `unix:PATH` or `pipe:NAME`.

use core::str::FromStr;
use std::net::{IpAddr, Ipv4Addr, TcpListener};

use bohio_client::forward::ForwardSettings;
use bohio_proto::forward::{BadTarget, Bind, Target};
use coqui_transport_pipe::{Listener, PipeName};

/// Text that is not a forward.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct BadSpec(String);

/// What a local listener listens on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Listen {
    /// An address and a port (0: the system chooses one).
    Tcp(IpAddr, u16),
    /// A Unix socket or Windows pipe.
    Ipc(PipeName),
}

/// One local forward: this machine listens, the host connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSpec {
    text: String,
    listen: Listen,
    target: Target,
}

/// A bound local listener.
#[derive(Debug)]
pub enum Bound {
    /// A TCP listener.
    Tcp(TcpListener),
    /// A Unix socket or pipe listener.
    Ipc(Listener),
}

impl FromStr for LocalSpec {
    type Err = BadSpec;

    fn from_str(text: &str) -> Result<Self, BadSpec> {
        let (listen, target) = text
            .split_once('=')
            .ok_or_else(|| BadSpec("write LISTEN=TARGET".to_owned()))?;
        let target: Target = target
            .parse()
            .map_err(|error: BadTarget| BadSpec(error.to_string()))?;
        let listen = if listen.starts_with("unix:") || listen.starts_with("pipe:") {
            Listen::Ipc(
                listen
                    .parse::<PipeName>()
                    .map_err(|error| BadSpec(error.to_string()))?,
            )
        } else {
            let (bind, port) = match listen.rsplit_once(':') {
                Some((bind, port)) => (Some(bind), port),
                None => (None, listen),
            };
            let port: u16 = port
                .parse()
                .map_err(|_| BadSpec("the port is a number from 0 to 65535".to_owned()))?;
            let address: IpAddr = match bind {
                None => Ipv4Addr::LOCALHOST.into(),
                Some(bind) => bind
                    .strip_prefix('[')
                    .and_then(|inner| inner.strip_suffix(']'))
                    .unwrap_or(bind)
                    .parse()
                    .map_err(|_| {
                        BadSpec("BIND is an address (an IPv6 one in brackets)".to_owned())
                    })?,
            };
            Listen::Tcp(address, port)
        };
        Ok(Self {
            text: text.to_owned(),
            listen,
            target,
        })
    }
}

impl LocalSpec {
    /// The entry as written.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Where the host connects.
    #[must_use]
    pub const fn target(&self) -> &Target {
        &self.target
    }

    /// The address the entry listens on when it is not loopback: whoever can reach it has what the peer's grant allows.
    #[must_use]
    pub fn non_default(&self) -> Option<IpAddr> {
        match &self.listen {
            Listen::Tcp(address, _) if !address.is_loopback() => Some(*address),
            _ => None,
        }
    }

    /// Binds the listener, and says what it is bound to (the port the system chose for port 0). A socket or pipe is for this
    /// user alone.
    ///
    /// # Errors
    ///
    /// Why it could not be bound.
    pub fn bind(&self, settings: &ForwardSettings) -> Result<(Bound, String), String> {
        match &self.listen {
            Listen::Tcp(address, port) => {
                let listener =
                    TcpListener::bind((*address, *port)).map_err(|error| error.to_string())?;
                let shown = listener
                    .local_addr()
                    .map_or_else(|_| format!("{address}:{port}"), |bound| bound.to_string());
                Ok((Bound::Tcp(listener), shown))
            }
            Listen::Ipc(name) => {
                let listener = bohio_client::forward::ipc::listener(name, &settings.options, None)
                    .map_err(|error| error.to_string())?;
                let shown = listener.name().to_string();
                Ok((Bound::Ipc(listener), shown))
            }
        }
    }
}

/// One remote forward: the host listens, this machine connects each arrival to `local`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSpec {
    text: String,
    bind: Bind,
    local: Target,
}

impl FromStr for RemoteSpec {
    type Err = BadSpec;

    fn from_str(text: &str) -> Result<Self, BadSpec> {
        let (listen, local) = text
            .split_once('=')
            .ok_or_else(|| BadSpec("write LISTEN=LOCAL".to_owned()))?;
        let local: Target = local
            .parse()
            .map_err(|error: BadTarget| BadSpec(error.to_string()))?;
        let bind =
            if listen.starts_with("unix:") || listen.starts_with("pipe:") || listen.contains(':') {
                listen
                    .parse::<Bind>()
                    .map_err(|error: BadTarget| BadSpec(error.to_string()))?
            } else {
                Bind::Tcp {
                    host: "127.0.0.1".to_owned(),
                    port: listen
                        .parse()
                        .map_err(|_| BadSpec("the port is a number from 0 to 65535".to_owned()))?,
                }
            };
        Ok(Self {
            text: text.to_owned(),
            bind,
            local,
        })
    }
}

impl RemoteSpec {
    /// The entry as written.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// What the host binds.
    #[must_use]
    pub const fn bind(&self) -> &Bind {
        &self.bind
    }

    /// Where each arrival is connected on this machine.
    #[must_use]
    pub const fn local(&self) -> &Target {
        &self.local
    }

    /// The address the host binds when it is not loopback.
    #[must_use]
    pub fn non_default(&self) -> Option<&str> {
        match &self.bind {
            Bind::Tcp { host, .. }
                if !host
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback()) =>
            {
                Some(host)
            }
            _ => None,
        }
    }
}

impl Bound {
    /// Serves the listener on `link` until `stop` is set or `link` is down: each connection is a connect to `target` and a pump.
    ///
    /// # Errors
    ///
    /// The listener's error.
    pub fn serve(
        &self,
        link: &dyn bohio_client::Open,
        target: &Target,
        settings: &ForwardSettings,
        stop: &std::sync::atomic::AtomicBool,
        report: &(dyn Fn(&bohio_client::Error) + Sync),
    ) -> std::io::Result<bohio_client::forward::Ended> {
        match self {
            Self::Tcp(listener) => {
                bohio_client::forward::listen_tcp(link, listener, target, settings, stop, report)
            }
            Self::Ipc(listener) => {
                bohio_client::forward::listen_ipc(link, listener, target, settings, stop, report)
            }
        }
    }
}
