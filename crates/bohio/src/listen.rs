//! Where a host accepts links: one listener per `[[listen]]` endpoint, whatever its transport.
//!
//! Each listener yields an unproven [`Channel`] (see `bohio_client::dial`); the proof, the link and the
//! engines that follow are the same for every scheme.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bohio_client::dial::{Endpoint, SerialOptions, web_limits};
use bohio_client::link::{Settings, socket, stream};
use coqui_peer_tls::{ClientAuth, Identity, ServerBuilder};
use coqui_transport::{Channel, Failure};
use coqui_transport_pipe::Listener as PipeListener;
use coqui_transport_web::websocket::{Frames, Listener as WebSocketListener, Pending, ServerTls};

use crate::config::ListenConfig;
use crate::serve::ServeError;

/// The TLS configuration of a `wss` entry: the certificate chain and key `tls_cert` and `tls_key`
/// name. The key is read here and never logged; a client presents no certificate, because the link's
/// proof, not TLS, says who it is.
///
/// # Errors
///
/// Why the files are missing or do not hold a certificate chain with its key.
pub fn server_config(config: &ListenConfig) -> Result<Arc<rustls::ServerConfig>, String> {
    let (Some(certificate), Some(key)) = (&config.tls_cert, &config.tls_key) else {
        return Err(format!(
            "`{}` needs tls_cert and tls_key: set them on its [[listen]] entry",
            config.endpoint
        ));
    };
    let read = |path: &Path| {
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
    };
    let mut pem = read(certificate)?;
    pem.push('\n');
    pem.push_str(&read(key)?);
    let identity = Identity::from_pem_chain(&pem).map_err(|_| {
        format!(
            "{} and {} do not hold a certificate chain and the key of its first certificate",
            certificate.display(),
            key.display()
        )
    })?;
    let built = ServerBuilder::new()
        .identity(&identity)
        .client_auth(ClientAuth::None)
        .build()
        .map_err(|error| error.to_string())?;
    Ok(Arc::new(built.config))
}

/// A bound listener.
#[derive(Debug)]
#[non_exhaustive]
pub enum Listening {
    /// A TCP socket.
    Tcp(TcpListener),
    /// A named pipe or a Unix socket.
    Pipe(PipeListener),
    /// A WebSocket, with TLS for `wss`.
    WebSocket(WebSocketListener),
    /// A serial port: there is one peer at the other end of the line, so nothing is bound; the port
    /// is opened for each link.
    Serial {
        /// The device.
        path: String,
        /// The line settings.
        options: SerialOptions,
    },
}

/// A connection that has arrived and whose channel is not open yet.
pub enum Arrival {
    /// A channel ready for the proof.
    Ready(Box<dyn Channel>, Option<SocketAddr>),
    /// A WebSocket whose handshake has not run: it runs on the connection's own thread, so one
    /// slow peer cannot hold the others up.
    Handshake(Box<Pending>),
}

impl Arrival {
    /// The address the peer connected from, when the transport has one.
    pub fn source(&self) -> Option<SocketAddr> {
        match self {
            Self::Ready(_, source) => *source,
            Self::Handshake(pending) => Some(pending.peer_addr()),
        }
    }

    /// The channel and the address the peer connected from, running a pending handshake.
    ///
    /// # Errors
    ///
    /// Why the connection could not be opened.
    pub fn open(self) -> io::Result<(Box<dyn Channel>, Option<SocketAddr>)> {
        match self {
            Self::Ready(channel, source) => Ok((channel, source)),
            Self::Handshake(pending) => {
                let source = pending.peer_addr();
                let channel = pending
                    .handshake()
                    .map_err(|failure| io::Error::other(failure.to_string()))?;
                Ok((Box::new(channel), Some(source)))
            }
        }
    }
}

impl Listening {
    /// Binds the endpoint `config` names, with the settings of its scheme from its keys; and the
    /// endpoint as bound (port 0 becomes the port chosen).
    ///
    /// # Errors
    ///
    /// [`ServeError::Listen`] for an endpoint that does not parse, is not one to listen on, or cannot
    /// be bound.
    pub fn bind(
        config: &ListenConfig,
        settings: &Settings,
    ) -> Result<(Self, Endpoint), ServeError> {
        let fail = |reason: String| ServeError::Listen {
            endpoint: config.endpoint.clone(),
            reason,
        };
        let endpoint = Endpoint::parse(&config.endpoint).map_err(fail)?;
        match endpoint {
            Endpoint::Tcp(address) => {
                let listener = TcpListener::bind(address).map_err(|e| fail(e.to_string()))?;
                listener.set_nonblocking(true).map_err(|e| fail(e.to_string()))?;
                let bound = listener.local_addr().map_err(|e| fail(e.to_string()))?;
                Ok((Self::Tcp(listener), Endpoint::Tcp(bound)))
            }
            Endpoint::Pipe(name) => {
                let mut builder = PipeListener::builder().name(&name);
                if let Some(bits) = config.socket_mode_bits().map_err(fail)? {
                    builder = builder.socket_mode(bits);
                }
                if let Some(remote) = config.remote_clients {
                    builder = builder.remote_clients(remote);
                }
                let listener = builder.build().map_err(|e| fail(e.to_string()))?;
                Ok((Self::Pipe(listener), Endpoint::Pipe(name)))
            }
            Endpoint::WebSocket(url) => {
                let address = url
                    .socket_addrs(|| None)
                    .map_err(|e| fail(e.to_string()))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| fail("the address does not resolve".to_owned()))?;
                let tls = if url.scheme() == "wss" {
                    ServerTls::Config(server_config(config).map_err(fail)?)
                } else {
                    ServerTls::Plain
                };
                let listener = WebSocketListener::builder(address, tls)
                    .limits(web_limits(settings))
                    .frames(Frames::Binary)
                    // The proof seals everything after it, so the carrier's own protection is not what the link relies on.
                    .plain_beyond_loopback(true)
                    .origins(config.origins.clone().unwrap_or_default())
                    .build()
                    .map_err(|e| fail(e.to_string()))?;
                let bound = listener.local_addr().map_err(|e| fail(e.to_string()))?;
                let url = Endpoint::parse(&format!("{}://{bound}", url.scheme())).map_err(fail)?;
                Ok((Self::WebSocket(listener), url))
            }
            Endpoint::Serial { path, options } => {
                let from_keys = config
                    .serial_options()
                    .map_err(|(key, reason)| fail(format!("{key}: {reason}")))?;
                let options = options.or(from_keys);
                let shown = Endpoint::Serial {
                    path: path.clone(),
                    options,
                };
                Ok((Self::Serial { path, options }, shown))
            }
            Endpoint::Command(_) => Err(fail(
                "a program is started to dial a host; to serve on standard input and output run `bohio serve --stdio`"
                    .to_owned(),
            )),
            _ => Err(fail("this build cannot listen there".to_owned())),
        }
    }

    /// Waits up to `wait` for a connection: `None` when none came.
    ///
    /// # Errors
    ///
    /// The listener's own failure, which ends it.
    pub fn accept(&self, wait: Duration, settings: &Settings) -> io::Result<Option<Arrival>> {
        match self {
            Self::Tcp(listener) => match listener.accept() {
                Ok((connection, source)) => {
                    connection.set_nonblocking(false)?;
                    let channel = socket(connection, settings)?;
                    Ok(Some(Arrival::Ready(Box::new(channel), Some(source))))
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(wait.min(Duration::from_millis(20)));
                    Ok(None)
                }
                Err(error) => Err(error),
            },
            Self::Pipe(listener) => match listener.accept_timeout(Some(wait)) {
                Ok(connection) => {
                    let channel = stream(connection, settings)?;
                    Ok(Some(Arrival::Ready(Box::new(channel), None)))
                }
                Err(coqui_transport_pipe::Error::TimedOut) => Ok(None),
                Err(error) => Err(error.into()),
            },
            Self::WebSocket(listener) => match listener.next(Some(wait)) {
                Ok(pending) => Ok(Some(Arrival::Handshake(Box::new(pending)))),
                Err(Failure::Timeout) => Ok(None),
                Err(failure) => {
                    // A refused source or a broken request is one connection's failure, not the listener's.
                    tracing::debug!("a WebSocket connection was not taken: {failure}");
                    thread::sleep(wait.min(Duration::from_millis(20)));
                    Ok(None)
                }
            },
            Self::Serial { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a serial line has one peer: it is served in turn, not accepted",
            )),
        }
    }
}
