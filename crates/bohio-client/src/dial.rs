//! Where a link goes: the endpoint text of each transport and the dialer that turns one into a channel.
//!
//! Every transport is one adapter that yields a [`Channel`] holding an unproven peer to one message of
//! the proof's size (see [`link::framed`](crate::link::framed)); the proof and the link run on it
//! unchanged, so nothing past this module names a transport.
//!
//! | Endpoint | Transport |
//! | --- | --- |
//! | `tcp://host:port` | a TCP socket |
//! | `pipe://NAME`, `pipe:///path/to.sock` | a Windows named pipe or a Unix domain socket |
//! | `ws://host:port[/path]`, `wss://host:port[/path]` | a WebSocket (TLS for `wss`), binary messages |
//! | `webtransport://host:port[/path]` | WebTransport over QUIC (UDP), always TLS; a WebSocket endpoint can follow when the path is blocked |
//! | `serial://PORT?baud=115200` | a serial port, COBS frames with a CRC |
//! | `stdio://PROGRAM ARG...` | a program started with its standard streams as the channel |

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cabuya::auth::MAX_PROOF_MESSAGE;
use coqui_peer::PeerId;
use coqui_peer_tls::{ClientBuilder, PeerServerVerifier, Pins, Policy, Roots, ServerAuth, Trust};
use coqui_transport::process::{ErrorOutput, Launch, ProcessBuilder};
use coqui_transport::{Channel, Connector as _, Failure};
use coqui_transport_pipe::PipeName;
use coqui_transport_serial::{
    Cobs, DataBits, FlowControl, Parity, Serial, SerialBuilder, StopBits,
};
use coqui_transport_web::websocket::{Connector as WebSocketConnector, Dialer, Frames};
use coqui_transport_web::webtransport::{self, ClientTrust, Connector as WebTransportConnector};
pub use coqui_transport_web::webtransport::{MIN_MTU, Mtu};
use coqui_transport_web::{Dialer as WebDialer, Endpoints, Kind, Limits, NoPreflight};
use url::Url;

use crate::Error;
use crate::certificate::{Checked, Refusals};
use crate::link::{Settings, framed, socket, stream};

/// The serial line settings an endpoint or a `[[listen]]` entry gives; a setting left out takes the port
/// crate's default (115200 baud, 8 data bits, no parity, 1 stop bit, no flow control).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SerialOptions {
    /// Bits per second. Not zero.
    pub baud: Option<u32>,
    /// Data bits per character, 5 to 8. The link's framing needs 8.
    pub data_bits: Option<DataBits>,
    /// The parity bit.
    pub parity: Option<Parity>,
    /// Stop bits after each character.
    pub stop_bits: Option<StopBits>,
    /// `none` or `hardware`: software flow control consumes bytes the framing carries.
    pub flow_control: Option<FlowControl>,
}

impl SerialOptions {
    /// Sets the setting `key` (`baud`, `data_bits`, `parity`, `stop_bits` or `flow_control`) from its text.
    ///
    /// # Errors
    ///
    /// The reason, for an unknown key or a value the setting does not take.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let bad = |what: &str| format!("`{value}` is not {what} for `{key}`");
        match key {
            "baud" => {
                self.baud = Some(
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|baud| *baud > 0)
                        .ok_or_else(|| bad("a bit rate above 0"))?,
                );
            }
            "data_bits" => {
                self.data_bits = Some(match value {
                    "5" => DataBits::Five,
                    "6" => DataBits::Six,
                    "7" => DataBits::Seven,
                    "8" => DataBits::Eight,
                    _ => return Err(bad("5, 6, 7 or 8")),
                });
            }
            "parity" => {
                self.parity = Some(match value {
                    "none" => Parity::None,
                    "odd" => Parity::Odd,
                    "even" => Parity::Even,
                    _ => return Err(bad("none, odd or even")),
                });
            }
            "stop_bits" => {
                self.stop_bits = Some(match value {
                    "1" => StopBits::One,
                    "2" => StopBits::Two,
                    _ => return Err(bad("1 or 2")),
                });
            }
            "flow_control" => {
                self.flow_control = Some(match value {
                    "none" => FlowControl::None,
                    "hardware" => FlowControl::Hardware,
                    _ => return Err(bad("none or hardware")),
                });
            }
            other => {
                return Err(format!(
                    "`{other}` is not a serial setting: baud, data_bits, parity, stop_bits, flow_control"
                ));
            }
        }
        Ok(())
    }

    /// These settings, with what `fallback` sets where this sets nothing.
    #[must_use]
    pub fn or(self, fallback: Self) -> Self {
        Self {
            baud: self.baud.or(fallback.baud),
            data_bits: self.data_bits.or(fallback.data_bits),
            parity: self.parity.or(fallback.parity),
            stop_bits: self.stop_bits.or(fallback.stop_bits),
            flow_control: self.flow_control.or(fallback.flow_control),
        }
    }
}

/// Whom a client accepts as the host's TLS certificate on `wss://`.
///
/// The link's proof, not TLS, says who the host is; this decides only whether the carrier's own
/// handshake succeeds. With nothing set the system's roots decide. CA files replace the system's
/// roots; pins accept the certificate they name whatever its name and issuer. With both, a
/// certificate that matches a pin or chains to a CA file is accepted.
///
/// A certificate is also held to its dates. `clock_skew` is how far this machine's clock may differ from the
/// certificate's maker: a certificate that starts, or ends, within it is accepted. Zero is strict, and then a
/// certificate made just now on a host whose clock is ahead is refused until this clock reaches its start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ServerTrust {
    /// PEM files whose certificates are the trusted roots, instead of the system's.
    pub ca_files: Vec<PathBuf>,
    /// SHA-256 digests of certificates trusted as they are, each `sha256:` and 64 hex digits.
    pub pins: Vec<PeerId>,
    /// How far the certificate's dates may be from this clock and still be accepted. Default zero.
    pub clock_skew: Duration,
    refusals: Refusals,
}

impl ServerTrust {
    /// Adds the pin `text` (`sha256:` and 64 hex digits).
    ///
    /// # Errors
    ///
    /// The reason, for text that is not a SHA-256 digest.
    pub fn pin(&mut self, text: &str) -> Result<(), String> {
        let id = text.trim().parse::<PeerId>().map_err(|_| {
            format!("`{text}` is not a certificate hash: write `sha256:` and 64 hex digits")
        })?;
        if !id.to_string().starts_with("sha256:") {
            return Err(format!("`{text}` is not a SHA-256 certificate hash"));
        }
        self.pins.push(id);
        Ok(())
    }

    /// Why the last certificate a configuration from this trust refused was refused, naming the certificate by its
    /// hash and, for its dates, the clock difference; `None` when none was.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        self.refusals.last()
    }

    /// The configuration a TLS client uses to check the host's certificate.
    ///
    /// # Errors
    ///
    /// Why a CA file cannot be read or holds no usable certificate.
    pub fn client_config(&self) -> Result<Arc<rustls::ClientConfig>, String> {
        let roots = if self.ca_files.is_empty() {
            Roots::platform()
        } else {
            let mut bundle = String::new();
            for path in &self.ca_files {
                let text = std::fs::read_to_string(path)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                bundle.push_str(&text);
                bundle.push('\n');
            }
            Roots::bundle_pem(bundle)
        };
        let trust = match (self.pins.is_empty(), self.ca_files.is_empty()) {
            (true, _) => Trust::Roots(roots),
            (false, true) => Trust::Pinned(Pins::fixed(self.pins.iter().copied())),
            (false, false) => Trust::PinnedOrRoots {
                pins: Pins::fixed(self.pins.iter().copied()),
                roots,
            },
        };
        let inner = PeerServerVerifier::new(&trust, &Policy::default())
            .map_err(|error| error.to_string())?;
        let checked = Checked {
            inner,
            skew: self.clock_skew,
            refusals: self.refusals.clone(),
        };
        let built = ClientBuilder::new()
            .server(ServerAuth::Verifier(Arc::new(checked)))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(built.config))
    }
}

/// A place to listen or connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Endpoint {
    /// A TCP socket.
    Tcp(SocketAddr),
    /// A named pipe or a Unix socket.
    Pipe(PipeName),
    /// A WebSocket, `ws://` or `wss://`.
    WebSocket(Url),
    /// WebTransport, `webtransport://host:port[/path]`: QUIC on a UDP port, always TLS.
    WebTransport {
        /// The endpoint, with its UDP port.
        url: Url,
        /// A WebSocket endpoint of the same host to dial when the UDP path fails before anything crossed
        /// (see [`Endpoint::with_fallback`]).
        fallback: Option<Url>,
        /// The UDP payload sizes and path-MTU discovery of this endpoint's QUIC connection (see
        /// [`Endpoint::with_mtu`]); the default is quinn's.
        mtu: Mtu,
    },
    /// A serial port: a device path on Unix, a port name such as `COM3` on Windows.
    Serial {
        /// The device.
        path: String,
        /// The line settings the endpoint's query gave.
        options: SerialOptions,
    },
    /// A program whose standard input and output are the channel: dial only.
    Command(Vec<String>),
}

impl Endpoint {
    /// Reads `scheme://address`; a name is resolved and its first address taken.
    ///
    /// # Errors
    ///
    /// The reason, for a scheme this build does not serve or an address that does not parse.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (scheme, rest) = text.split_once("://").ok_or_else(|| {
            "an endpoint looks like `tcp://host:port`, `pipe://name`, `ws://host:port`, `wss://host:port`, `webtransport://host:port`, `serial://port?baud=N` or `stdio://program args`"
                .to_owned()
        })?;
        match scheme {
            "tcp" => first_address(rest).map(Self::Tcp),
            "pipe" => {
                let named = if rest.contains(['/', '\\']) {
                    PipeName::at(rest)
                } else {
                    PipeName::named(rest)
                };
                named
                    .map(Self::Pipe)
                    .map_err(|error| format!("`{rest}` is not a pipe name: {error}"))
            }
            "ws" | "wss" => {
                let url =
                    Url::parse(text).map_err(|error| format!("`{text}` is not a URL: {error}"))?;
                if url.host_str().is_none() || url.port_or_known_default().is_none() {
                    return Err(format!("`{text}` names no host"));
                }
                Ok(Self::WebSocket(url))
            }
            "webtransport" => {
                let url =
                    Url::parse(text).map_err(|error| format!("`{text}` is not a URL: {error}"))?;
                if url.host_str().is_none() || url.port().is_none() {
                    return Err(format!(
                        "`{text}` needs a host and a UDP port: `webtransport://host:4433`"
                    ));
                }
                Ok(Self::WebTransport {
                    url,
                    fallback: None,
                    mtu: Mtu::default(),
                })
            }
            "serial" => {
                let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
                if path.is_empty() {
                    return Err("`serial://` needs a port: `serial:///dev/ttyUSB0?baud=115200` or `serial://COM3`".to_owned());
                }
                let mut options = SerialOptions::default();
                for pair in query.split('&').filter(|pair| !pair.is_empty()) {
                    let (key, value) = pair
                        .split_once('=')
                        .ok_or_else(|| format!("`{pair}` is not `key=value`"))?;
                    options.set(key, value)?;
                }
                Ok(Self::Serial {
                    path: path.to_owned(),
                    options,
                })
            }
            "stdio" => split_command(rest).map(Self::Command),
            other => Err(format!(
                "the scheme `{other}` is not served; use tcp, pipe, ws, wss, webtransport, serial or stdio"
            )),
        }
    }

    /// This WebTransport endpoint with `fallback`, a `ws://` or `wss://` endpoint, dialled when the QUIC
    /// path fails before anything crossed (a blocked UDP port, a timeout). A certificate, permission or
    /// protocol failure never falls back.
    ///
    /// Naming the fallback is the choice: a `ws://` one is dialled plain on any address, and one on
    /// another host than the WebTransport endpoint is dialled there. A plain WebSocket is readable and
    /// changeable by anything on the path until the link's proof, which seals what follows, has run; a
    /// fallback to another host sends the proof to that host, so the peer it proves is the one the
    /// caller expects only if the link's trust says so.
    ///
    /// # Errors
    ///
    /// The reason, when this is not a WebTransport endpoint or `fallback` is not a WebSocket one.
    pub fn with_fallback(self, fallback: Self) -> Result<Self, String> {
        match (self, fallback) {
            (Self::WebTransport { url, mtu, .. }, Self::WebSocket(other)) => {
                Ok(Self::WebTransport {
                    url,
                    fallback: Some(other),
                    mtu,
                })
            }
            _ => Err(
                "a fallback is a `ws://` or `wss://` endpoint that follows a `webtransport://` one"
                    .to_owned(),
            ),
        }
    }

    /// This WebTransport endpoint with `mtu` as the UDP payload sizes and path-MTU discovery of its QUIC connection. The
    /// default is quinn's (start at 1200, probe up to 1452); a path with a smaller MTU, such as a tunnel, loses each probe
    /// above it and a host may log that as a send error, so lower `upper_bound` to the path's size or turn `discovery` off.
    /// A larger `initial` than the path carries stalls the connection until black-hole detection falls to `min`. See
    /// [`Mtu`].
    ///
    /// # Errors
    ///
    /// The reason, when this is not a WebTransport endpoint or `initial` or `min` is below [`MIN_MTU`], which names the value.
    pub fn with_mtu(self, mtu: Mtu) -> Result<Self, String> {
        mtu.check().map_err(|error| error.to_string())?;
        match self {
            Self::WebTransport { url, fallback, .. } => {
                Ok(Self::WebTransport { url, fallback, mtu })
            }
            other => Err(format!(
                "the MTU settings are for a `webtransport://` endpoint, not `{other}`"
            )),
        }
    }

    /// Opens a channel to this endpoint, an unproven channel to the host, within `timeout`.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] with why the path could not be opened.
    pub fn dial(&self, timeout: Duration, settings: &Settings) -> Result<Box<dyn Channel>, Error> {
        self.dial_with(timeout, settings, &ServerTrust::default())
    }

    /// As [`Endpoint::dial`], checking a TLS host's certificate by `trust`.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] with why the path could not be opened (a certificate the trust refuses is
    /// `Failure::Certificate`), [`Error::Link`] when the trust cannot be set up.
    pub fn dial_with(
        &self,
        timeout: Duration,
        settings: &Settings,
        trust: &ServerTrust,
    ) -> Result<Box<dyn Channel>, Error> {
        match self {
            Self::Tcp(address) => {
                let stream = crate::link::connect(address, timeout, settings)
                    .map_err(|error| io_failure(&error))?;
                // A loopback connect to a closed port in the ephemeral range can connect a socket to itself.
                if stream.local_addr().ok() == stream.peer_addr().ok() {
                    return Err(Error::Transport(Failure::PathLost));
                }
                let channel = socket(stream, settings).map_err(|error| io_failure(&error))?;
                Ok(Box::new(channel))
            }
            Self::Pipe(name) => {
                let pipe = coqui_transport_pipe::Connector::new(name.clone())
                    .connect(timeout)
                    .map_err(|error| io_failure(&io::Error::from(error)))?;
                let channel = stream(pipe, settings).map_err(|error| io_failure(&error))?;
                Ok(Box::new(channel))
            }
            Self::WebSocket(url) => {
                let dialer = web_socket_dialer(url, settings, trust, true)?;
                let channel = WebSocketConnector::new(dialer, url.clone())
                    .connect(timeout)
                    .map_err(Error::Transport)?;
                Ok(Box::new(channel))
            }
            Self::WebTransport { url, fallback, mtu } => {
                dial_web_transport(url, fallback.as_ref(), *mtu, timeout, settings, trust)
            }
            Self::Serial { path, options } => {
                let channel =
                    serial(path, options, settings).map_err(|error| io_failure(&error))?;
                Ok(Box::new(channel))
            }
            Self::Command(argv) => {
                let channel = command(argv, settings).map_err(|error| io_failure(&error))?;
                Ok(Box::new(channel))
            }
        }
    }
}

impl core::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Tcp(address) => write!(f, "tcp://{address}"),
            Self::Pipe(name) => write!(f, "pipe://{}", name.path().display()),
            Self::WebSocket(url) => write!(f, "{url}"),
            Self::WebTransport { url, fallback, .. } => {
                write!(f, "{url}")?;
                match fallback {
                    Some(fallback) => write!(f, " (or {fallback})"),
                    None => Ok(()),
                }
            }
            Self::Serial { path, options } => {
                write!(f, "serial://{path}")?;
                match options.baud {
                    Some(baud) => write!(f, "?baud={baud}"),
                    None => Ok(()),
                }
            }
            Self::Command(argv) => {
                f.write_str("stdio://")?;
                for (index, argument) in argv.iter().enumerate() {
                    if index > 0 {
                        f.write_str(" ")?;
                    }
                    if argument.is_empty() || argument.contains([' ', '"', '\'']) {
                        write!(f, "\"{}\"", argument.replace('"', "'"))?;
                    } else {
                        f.write_str(argument)?;
                    }
                }
                Ok(())
            }
        }
    }
}

/// A WebSocket dialer for `url`: TLS from `trust` for `wss`, binary frames, the unproven peer limits.
/// `plain_beyond_loopback` lets `ws://` reach any address (the proof seals what follows).
fn web_socket_dialer(
    url: &Url,
    settings: &Settings,
    trust: &ServerTrust,
    plain_beyond_loopback: bool,
) -> Result<Dialer, Error> {
    let mut builder = Dialer::builder();
    if url.scheme() == "wss" {
        builder = builder.tls(trust.client_config().map_err(Error::Link)?);
    }
    builder
        .frames(Frames::Binary)
        .limits(web_limits(settings))
        .plain_beyond_loopback(plain_beyond_loopback)
        .build()
        .map_err(|_| Error::Transport(Failure::Unsupported))
}

/// Opens WebTransport at `url` and, when it fails on the path before anything crossed and `fallback`
/// names a WebSocket endpoint, that.
fn dial_web_transport(
    url: &Url,
    fallback: Option<&Url>,
    mtu: Mtu,
    timeout: Duration,
    settings: &Settings,
    trust: &ServerTrust,
) -> Result<Box<dyn Channel>, Error> {
    // The crate names a WebTransport endpoint by `https`; this crate names it by what it is.
    let https = Url::parse(&url.as_str().replacen("webtransport", "https", 1))
        .map_err(|_| Error::Transport(Failure::Unsupported))?;
    let dialer = web_transport_dialer(mtu, settings, trust)?;
    let Some(fallback) = fallback else {
        let channel = WebTransportConnector::new(dialer, https)
            .connect(timeout)
            .map_err(Error::Transport)?;
        return Ok(Box::new(channel));
    };
    // The caller named this fallback, so a plain one and one on another host are what was asked for.
    let endpoints = Endpoints::builder()
        .webtransport(https.as_str())
        .websocket(fallback.as_str())
        .allow_plain_websocket(fallback.scheme() == "ws")
        .require_same_host(https.host() == fallback.host())
        .build()
        .map_err(|error| Error::Link(format!("the endpoint pair is refused: {error:?}")))?;
    let mut both = Both {
        websocket: web_socket_dialer(fallback, settings, trust, true)?,
        webtransport: dialer,
    };
    let opened = coqui_transport_web::connect(&endpoints, &mut both, &mut NoPreflight, timeout)
        .map_err(Error::Transport)?;
    // The caller proves the peer next, on this channel, before anything else crosses it.
    Ok(opened.into_verified())
}

fn web_transport_dialer(
    mtu: Mtu,
    settings: &Settings,
    trust: &ServerTrust,
) -> Result<webtransport::Dialer, Error> {
    // The hash decides, whatever the certificate's key or validity: ClientTrust::Pinned would hold it to what a
    // browser accepts (ECDSA P-256, at most 14 days), which is the browser's rule and not this client's.
    webtransport::Dialer::builder(ClientTrust::Config(
        trust.client_config().map_err(Error::Link)?,
    ))
    .limits(web_limits(settings))
    .mtu(mtu)
    .build()
    .map_err(|_| Error::Transport(Failure::Unsupported))
}

/// Dials WebTransport or WebSocket by the kind [`coqui_transport_web::connect`] asks for.
struct Both {
    websocket: Dialer,
    webtransport: webtransport::Dialer,
}

impl WebDialer for Both {
    type Connection = Box<dyn Channel>;

    fn dial(
        &mut self,
        kind: Kind,
        endpoint: &Url,
        timeout: Duration,
    ) -> Result<Self::Connection, Failure> {
        match kind {
            Kind::WebSocket => Ok(Box::new(self.websocket.dial(kind, endpoint, timeout)?)),
            _ => Ok(Box::new(self.webtransport.dial(kind, endpoint, timeout)?)),
        }
    }
}

fn first_address(text: &str) -> Result<SocketAddr, String> {
    text.to_socket_addrs()
        .map_err(|error| format!("`{text}` cannot be resolved: {error}"))?
        .next()
        .ok_or_else(|| format!("`{text}` has no address"))
}

/// The failure an I/O error on the way to a host reports.
fn io_failure(error: &io::Error) -> Error {
    Error::Transport(match error.kind() {
        io::ErrorKind::PermissionDenied => Failure::Permission,
        io::ErrorKind::TimedOut => Failure::Timeout,
        io::ErrorKind::Unsupported => Failure::Unsupported,
        _ => Failure::PathLost,
    })
}

/// The limits a WebSocket link holds an unproven peer to: one message of the proof's size, read
/// ahead one at a time, until the proof raises them.
#[must_use]
pub fn web_limits(settings: &Settings) -> Limits {
    let mut limits = Limits::default();
    limits.max_message = MAX_PROOF_MESSAGE;
    limits.pending = 1;
    limits.queued_bytes = settings.queued_bytes.max(1);
    limits.send = settings.send_limit;
    limits
}

/// Opens the serial port `path` with `options` as a channel: COBS frames, each with a CRC-32C, so a
/// damaged frame is dropped and the link's own checks see a gap.
///
/// # Errors
///
/// The error of opening the port or starting the channel's threads.
pub fn serial(
    path: &str,
    options: &SerialOptions,
    settings: &Settings,
) -> io::Result<coqui_transport::StreamChannel> {
    let port = open_port(path, options)?;
    framed(port, Cobs::new(), settings)
}

fn open_port(path: &str, options: &SerialOptions) -> io::Result<Serial> {
    let mut builder = SerialBuilder::new().path(path);
    if let Some(baud) = options.baud {
        builder = builder.baud_rate(baud);
    }
    if let Some(bits) = options.data_bits {
        builder = builder.data_bits(bits);
    }
    if let Some(parity) = options.parity {
        builder = builder.parity(parity);
    }
    if let Some(stop) = options.stop_bits {
        builder = builder.stop_bits(stop);
    }
    if let Some(flow) = options.flow_control {
        builder = builder.flow_control(flow);
    }
    builder
        .build()
        .map_err(|error| io::Error::other(error.to_string()))
}

/// Starts `argv` (run directly, never through a shell) with its standard input and output as a
/// channel and its standard error this process's. The program's whole tree ends when the channel
/// closes.
///
/// # Errors
///
/// The error of starting the program or the channel's threads.
pub fn command(argv: &[String], settings: &Settings) -> io::Result<coqui_transport::StreamChannel> {
    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no program to start"))?;
    let process = ProcessBuilder::new()
        .launch(Launch::new(program).args(arguments))
        .stderr(ErrorOutput::Inherit)
        .build()
        .map_err(|error| io::Error::other(error.to_string()))?;
    stream(process, settings)
}

/// Splits a command line into an argument vector: spaces separate arguments, `"` or `'` groups
/// characters (a quote is dropped), and nothing is an escape, so a Windows path stays as written.
///
/// # Errors
///
/// The reason, for an empty line or a quote that is not closed.
pub fn split_command(line: &str) -> Result<Vec<String>, String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for character in line.chars() {
        match (quote, character) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(character);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    arguments.push(core::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("a quote in the command is not closed".to_owned());
    }
    if started {
        arguments.push(current);
    }
    if arguments.is_empty() {
        return Err("the command is empty".to_owned());
    }
    Ok(arguments)
}
