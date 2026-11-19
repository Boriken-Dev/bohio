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
//! | `ws://host:port[/path]` | a WebSocket, binary messages |
//! | `serial://PORT?baud=115200` | a serial port, COBS frames with a CRC |
//! | `stdio://PROGRAM ARG...` | a program started with its standard streams as the channel |

use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use coqui_link::auth::MAX_PROOF_MESSAGE;
use coqui_transport::process::{ErrorOutput, Launch, ProcessBuilder};
use coqui_transport::{Channel, Connector as _, Failure};
use coqui_transport_pipe::PipeName;
use coqui_transport_serial::{
    Cobs, DataBits, FlowControl, Parity, Serial, SerialBuilder, StopBits,
};
use coqui_transport_web::Limits;
use coqui_transport_web::websocket::{Connector as WebSocketConnector, Dialer, Frames};
use url::Url;

use crate::Error;
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

/// A place to listen or connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Endpoint {
    /// A TCP socket.
    Tcp(SocketAddr),
    /// A named pipe or a Unix socket.
    Pipe(PipeName),
    /// A WebSocket, `ws://`.
    WebSocket(Url),
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
            "an endpoint looks like `tcp://host:port`, `pipe://name`, `ws://host:port`, `serial://port?baud=N` or `stdio://program args`"
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
            "ws" => {
                let url = Url::parse(text).map_err(|error| format!("`{text}` is not a URL: {error}"))?;
                if url.host_str().is_none() || url.port_or_known_default().is_none() {
                    return Err(format!("`{text}` names no host"));
                }
                Ok(Self::WebSocket(url))
            }
            "wss" => Err("`wss` is not served by this build: TLS for WebSocket endpoints is not set up; use `ws` (the link is sealed by the proof)".to_owned()),
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
                "the scheme `{other}` is not served; use tcp, pipe, ws, serial or stdio"
            )),
        }
    }

    /// Opens a channel to this endpoint, an unproven channel to the host, within `timeout`.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] with why the path could not be opened.
    pub fn dial(&self, timeout: Duration, settings: &Settings) -> Result<Box<dyn Channel>, Error> {
        match self {
            Self::Tcp(address) => {
                let stream = TcpStream::connect_timeout(address, timeout)
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
                let dialer = Dialer::builder()
                    .frames(Frames::Binary)
                    .limits(web_limits(settings))
                    // The proof seals everything after it, so the carrier's own protection is not what the link relies on.
                    .plain_beyond_loopback(true)
                    .build()
                    .map_err(|_| Error::Transport(Failure::Unsupported))?;
                let channel = WebSocketConnector::new(dialer, url.clone())
                    .connect(timeout)
                    .map_err(Error::Transport)?;
                Ok(Box::new(channel))
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
