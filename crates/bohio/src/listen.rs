//! Where a host accepts links: one listener per `[[listen]]` endpoint, whatever its transport.
//!
//! Each listener yields an unproven [`Channel`] (see `bohio_client::dial`); the proof, the link and the
//! engines that follow are the same for every scheme.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use bohio_client::dial::{Endpoint, SerialOptions, web_limits};
use bohio_client::link::{Settings, socket, stream};
use coqui_peer_tls::{ClientAuth, Identity, ServerBuilder};
use coqui_transport::{Channel, Failure};
use coqui_transport_pipe::Listener as PipeListener;
use coqui_transport_web::websocket::{Frames, Listener as WebSocketListener, Pending, ServerTls};
use coqui_transport_web::webtransport::{
    Listener as WebTransportListener, Pending as WebTransportPending, ServerTls as WebTransportTls,
};

use crate::config::ListenConfig;
use crate::node::NodeError;

/// How many days a certificate the host makes for WebTransport is valid when the entry says nothing:
/// a browser pinning it by hash accepts at most 14, and a day short of that leaves room for a slow clock.
const DEFAULT_CERTIFICATE_DAYS: u32 = 13;

/// How long before now a certificate the host makes starts to be valid when the entry says nothing.
const DEFAULT_BACKDATE: Duration = Duration::from_secs(3600);

/// The longest validity, in days, a browser accepts for a certificate pinned by hash (`serverCertificateHashes`).
const BROWSER_PIN_DAYS: u64 = 14;

/// Why a browser would not accept `certificate_der` pinned by its hash, or `None` when it would: the browsers'
/// rule is an ECDSA P-256 key and a validity of at most 14 days. A client that is not a browser pins any
/// certificate, so this is a report and never a refusal.
#[must_use]
pub fn browser_pin_problem(certificate_der: &[u8]) -> Option<String> {
    let Ok((_, certificate)) = x509_parser::parse_x509_certificate(certificate_der) else {
        return Some("the certificate could not be read".to_owned());
    };
    let validity = certificate.validity();
    let seconds = validity.not_after.timestamp() - validity.not_before.timestamp();
    let public_key = certificate.public_key();
    let p256 = public_key
        .algorithm
        .parameters
        .as_ref()
        .and_then(|parameters| parameters.as_oid().ok())
        .is_some_and(|curve| curve.to_id_string() == "1.2.840.10045.3.1.7");
    let ecdsa = public_key.algorithm.algorithm.to_id_string() == "1.2.840.10045.2.1";
    let mut problems = Vec::new();
    if !(ecdsa && p256) {
        problems.push("its key is not ECDSA P-256".to_owned());
    }
    if seconds > i64::try_from(BROWSER_PIN_DAYS * 24 * 3600).unwrap_or(i64::MAX) {
        problems.push(format!(
            "it is valid for {} days, more than {BROWSER_PIN_DAYS}",
            seconds / 86400
        ));
    }
    (!problems.is_empty()).then(|| problems.join(" and "))
}

/// The TLS configuration of a `wss` entry: the certificate chain and key `tls_cert` and `tls_key`
/// name. The key is read here and never logged; a client presents no certificate, because the link's
/// proof, not TLS, says who it is.
///
/// # Errors
///
/// Why the files are missing or do not hold a certificate chain with its key.
pub fn server_config(config: &ListenConfig) -> Result<Arc<rustls::ServerConfig>, String> {
    server_config_of(&server_identity(config)?)
}

/// The certificate chain and key `tls_cert` and `tls_key` name.
fn server_identity(config: &ListenConfig) -> Result<Identity, String> {
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
    Identity::from_pem_chain(&pem).map_err(|_| {
        format!(
            "{} and {} do not hold a certificate chain and the key of its first certificate",
            certificate.display(),
            key.display()
        )
    })
}

fn server_config_of(identity: &Identity) -> Result<Arc<rustls::ServerConfig>, String> {
    let built = ServerBuilder::new()
        .identity(identity)
        .client_auth(ClientAuth::None)
        .build()
        .map_err(|error| error.to_string())?;
    Ok(Arc::new(built.config))
}

/// The self-signed certificate a WebTransport listener makes when its entry names none: ECDSA P-256,
/// valid for the days the entry gives, so a browser can pin it by hash (`serverCertificateHashes`).
/// A new one is made when half the validity has passed; connections that follow get it, and its hash
/// is logged.
#[derive(Debug)]
pub struct MadeCertificate {
    validity: Duration,
    backdate: Duration,
    current: Mutex<Made>,
}

#[derive(Debug)]
struct Made {
    config: Arc<rustls::ServerConfig>,
    hash: String,
    renew_at: Instant,
}

impl MadeCertificate {
    /// A certificate valid for `validity`, started `backdate` ago so a client whose clock is a little
    /// behind accepts it.
    ///
    /// # Errors
    ///
    /// Why a certificate cannot be made.
    pub fn new(validity: Duration, backdate: Duration) -> Result<Arc<Self>, String> {
        let current = Self::make(validity, backdate)?;
        Ok(Arc::new(Self {
            validity,
            backdate,
            current: Mutex::new(current),
        }))
    }

    fn make(validity: Duration, backdate: Duration) -> Result<Made, String> {
        let identity = Identity::builder()
            .not_before(SystemTime::now() - backdate)
            .valid_for(validity)
            .generate()
            .map_err(|error| format!("a certificate cannot be made: {error}"))?;
        Ok(Made {
            config: server_config_of(&identity)?,
            hash: identity.fingerprint().to_string(),
            renew_at: Instant::now() + validity / 2,
        })
    }

    /// The configuration for the next connection, made anew when the certificate is half spent.
    pub fn config(&self) -> Arc<rustls::ServerConfig> {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Instant::now() >= current.renew_at {
            match Self::make(self.validity, self.backdate) {
                Ok(made) => {
                    tracing::info!(certificate = %made.hash, "a new WebTransport certificate was made");
                    *current = made;
                }
                // The old one still serves until it expires; the next connection tries again.
                Err(error) => {
                    tracing::warn!("a new WebTransport certificate was not made: {error}")
                }
            }
        }
        Arc::clone(&current.config)
    }

    /// The SHA-256 of the certificate now in use, `sha256:` and 64 hex digits.
    pub fn hash(&self) -> String {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .hash
            .clone()
    }
}

/// Where a WebTransport listener's certificate comes from.
#[derive(Debug)]
pub enum Certificate {
    /// The files `tls_cert` and `tls_key` name; the hash is of the first certificate.
    Files(String),
    /// A certificate the host makes and renews.
    Made(Arc<MadeCertificate>),
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
    /// WebTransport over QUIC.
    WebTransport {
        /// The listener.
        listener: WebTransportListener,
        /// Its certificate.
        certificate: Certificate,
    },
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
    /// A WebTransport session whose handshake has not run, on the connection's own thread too.
    SessionHandshake(Box<WebTransportPending>),
}

impl Arrival {
    /// The address the peer connected from, when the transport has one.
    pub fn source(&self) -> Option<SocketAddr> {
        match self {
            Self::Ready(_, source) => *source,
            Self::Handshake(pending) => Some(pending.peer_addr()),
            Self::SessionHandshake(pending) => Some(pending.peer_addr()),
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
            Self::SessionHandshake(pending) => {
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
    /// [`NodeError::Listen`] for an endpoint that does not parse, is not one to listen on, or cannot
    /// be bound.
    pub fn bind(config: &ListenConfig, settings: &Settings) -> Result<(Self, Endpoint), NodeError> {
        let fail = |reason: String| NodeError::Listen {
            endpoint: config.endpoint.clone(),
            reason,
        };
        let endpoint = Endpoint::parse(&config.endpoint).map_err(fail)?;
        match endpoint {
            Endpoint::Tcp(address) => {
                let listener = TcpListener::bind(address).map_err(|e| fail(e.to_string()))?;
                listener.set_nonblocking(true).map_err(|e| fail(e.to_string()))?;
                bohio_client::link::tune_listener(&listener, settings).map_err(|e| fail(e.to_string()))?;
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
            Endpoint::WebTransport { url, .. } => {
                let address = url
                    .socket_addrs(|| None)
                    .map_err(|e| fail(e.to_string()))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| fail("the address does not resolve".to_owned()))?;
                let (tls, certificate) = if config.tls_cert.is_some() {
                    let identity = server_identity(config).map_err(fail)?;
                    let hash = identity.fingerprint().to_string();
                    if let Some(problem) = browser_pin_problem(identity.certificate_der()) {
                        tracing::warn!(certificate = %hash, "a browser will not accept this WebTransport certificate pinned by its hash: {problem}");
                    }
                    let tls = WebTransportTls::Config(server_config_of(&identity).map_err(fail)?);
                    (tls, Certificate::Files(hash))
                } else {
                    let days = u64::from(config.tls_valid_days.unwrap_or(DEFAULT_CERTIFICATE_DAYS));
                    if days > BROWSER_PIN_DAYS {
                        tracing::warn!("the WebTransport certificate is valid for {days} days: a browser will not accept it pinned by its hash, which takes at most {BROWSER_PIN_DAYS}");
                    }
                    let made = MadeCertificate::new(
                        Duration::from_secs(days * 24 * 3600),
                        config
                            .tls_backdate
                            .map_or(DEFAULT_BACKDATE, |span| span.0),
                    )
                    .map_err(fail)?;
                    let renewing = Arc::clone(&made);
                    let tls = WebTransportTls::per_connection(move |_, _| renewing.config());
                    (tls, Certificate::Made(made))
                };
                let listener = WebTransportListener::builder(address, tls)
                    .limits(web_limits(settings))
                    .origins(config.origins.clone().unwrap_or_default())
                    .build()
                    .map_err(|e| fail(e.to_string()))?;
                let bound = listener.local_addr();
                let shown =
                    Endpoint::parse(&format!("webtransport://{bound}")).map_err(fail)?;
                Ok((
                    Self::WebTransport {
                        listener,
                        certificate,
                    },
                    shown,
                ))
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

    /// The SHA-256 of the certificate a WebTransport listener presents now, which a client pins with
    /// `--cert-hash` (a browser with `serverCertificateHashes`). A listener that makes its own
    /// certificate changes it halfway through its validity.
    pub fn certificate_hash(&self) -> Option<String> {
        match self {
            Self::WebTransport { certificate, .. } => Some(match certificate {
                Certificate::Files(hash) => hash.clone(),
                Certificate::Made(made) => made.hash(),
            }),
            _ => None,
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
            Self::WebTransport { listener, .. } => match listener.next(Some(wait)) {
                Ok(pending) => Ok(Some(Arrival::SessionHandshake(Box::new(pending)))),
                Err(Failure::Timeout) => Ok(None),
                Err(failure) => {
                    // A refused source or a broken request is one connection's failure, not the listener's.
                    tracing::debug!("a WebTransport connection was not taken: {failure}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn made_for(days: u64) -> Vec<u8> {
        Identity::builder()
            .valid_for(Duration::from_secs(days * 24 * 3600))
            .generate()
            .unwrap()
            .certificate_der()
            .to_vec()
    }

    #[test]
    fn a_certificate_a_browser_would_pin_is_not_reported() {
        assert_eq!(browser_pin_problem(&made_for(14)), None);
        assert_eq!(browser_pin_problem(&made_for(1)), None);
    }

    #[test]
    fn a_certificate_a_browser_would_not_pin_says_why_and_is_still_a_certificate() {
        let long = browser_pin_problem(&made_for(30)).expect("30 days is too long");
        assert!(
            long.contains("30 days") && !long.contains("ECDSA"),
            "{long}"
        );
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let other = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let both = browser_pin_problem(other.der()).expect("Ed25519 is not ECDSA P-256");
        assert!(both.contains("not ECDSA P-256"), "{both}");
        assert!(browser_pin_problem(b"not a certificate").is_some());
    }

    #[test]
    fn a_made_certificate_is_replaced_when_half_its_validity_has_passed() {
        let made = MadeCertificate::new(Duration::from_secs(4), Duration::ZERO).unwrap();
        let first = made.hash();
        assert!(first.starts_with("sha256:") && first.len() == "sha256:".len() + 64);
        // The same certificate serves until it is half spent.
        let _ = made.config();
        assert_eq!(made.hash(), first);
        thread::sleep(Duration::from_millis(2300));
        let _ = made.config();
        assert_ne!(made.hash(), first, "the certificate was not renewed");
    }
}
