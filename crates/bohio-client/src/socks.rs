//! A SOCKS5 server whose connections are `bohio-connect/1` streams (`ssh -D`).
//!
//! [`serve`] listens on a local socket and, for each client that asks to CONNECT (RFC 1928, method 0: no authentication),
//! has the host connect to the target and answers the client only after the host did, so the reply code says what happened.
//! A domain name is passed to the host unresolved: the host's resolver answers, not this machine's. BIND and UDP ASSOCIATE
//! are answered "command not supported".
//!
//! Whoever can connect to the listener has what this peer is granted: bind it to loopback.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use bohio_proto::ErrorCode;
use bohio_proto::forward::Target;
use coqui_transport::Failure;

use crate::forward::{Ended, ForwardSettings, connect};
use crate::{Error, Open};

/// The version byte of SOCKS5.
const VERSION: u8 = 5;
/// The method "no authentication required".
const NO_AUTH: u8 = 0;
/// The answer to a greeting that offers no method this server uses.
const NO_ACCEPTABLE_METHOD: u8 = 0xFF;
/// The command CONNECT.
const CONNECT: u8 = 1;

/// Reply codes of RFC 1928 section 6.
pub mod reply {
    /// Succeeded.
    pub const SUCCEEDED: u8 = 0x00;
    /// General SOCKS server failure.
    pub const FAILURE: u8 = 0x01;
    /// Connection not allowed by ruleset: the peer's grant does not cover the target.
    pub const NOT_ALLOWED: u8 = 0x02;
    /// Network or host unreachable, or a name that does not resolve.
    pub const UNREACHABLE: u8 = 0x04;
    /// Connection refused.
    pub const REFUSED: u8 = 0x05;
    /// The host's connect timed out (TTL expired).
    pub const TIMEOUT: u8 = 0x06;
    /// Command not supported: BIND and UDP ASSOCIATE.
    pub const COMMAND_NOT_SUPPORTED: u8 = 0x07;
    /// Address type not supported.
    pub const ADDRESS_NOT_SUPPORTED: u8 = 0x08;
}

/// The reply code that tells a client why `error` ended its CONNECT.
#[must_use]
pub const fn reply_code(error: &Error) -> u8 {
    match error {
        Error::Remote(body) => match body.code {
            ErrorCode::Denied | ErrorCode::OutsideRoot => reply::NOT_ALLOWED,
            ErrorCode::Unreachable | ErrorCode::NotFound => reply::UNREACHABLE,
            ErrorCode::Refused => reply::REFUSED,
            ErrorCode::Timeout => reply::TIMEOUT,
            _ => reply::FAILURE,
        },
        // The open itself was refused: the grant holds no rule for the target, or it was revoked.
        Error::Transport(Failure::Permission) => reply::NOT_ALLOWED,
        Error::Timeout => reply::TIMEOUT,
        _ => reply::FAILURE,
    }
}

/// What a client asked to CONNECT to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asked {
    /// A target to connect.
    Connect(Target),
    /// A reply code to answer with and end.
    Refuse(u8),
}

/// Reads exactly `buffer.len()` bytes before `deadline`.
fn read_by(stream: &mut TcpStream, buffer: &mut [u8], deadline: Instant) -> io::Result<()> {
    let mut done = 0;
    while done < buffer.len() {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        stream.set_read_timeout(Some(left))?;
        match stream.read(&mut buffer[done..]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(read) => done += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Reads a client's greeting and request: what it asks to connect to, or the code to refuse with.
fn handshake(stream: &mut TcpStream, deadline: Instant) -> io::Result<Asked> {
    let mut head = [0_u8; 2];
    read_by(stream, &mut head, deadline)?;
    if head[0] != VERSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not SOCKS5"));
    }
    let mut methods = vec![0_u8; usize::from(head[1])];
    read_by(stream, &mut methods, deadline)?;
    if !methods.contains(&NO_AUTH) {
        stream.write_all(&[VERSION, NO_ACCEPTABLE_METHOD])?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the client offers no method this server accepts",
        ));
    }
    stream.write_all(&[VERSION, NO_AUTH])?;
    let mut request = [0_u8; 4];
    read_by(stream, &mut request, deadline)?;
    if request[0] != VERSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not SOCKS5"));
    }
    let host = match request[3] {
        1 => {
            let mut octets = [0_u8; 4];
            read_by(stream, &mut octets, deadline)?;
            Some(IpAddr::from(octets).to_string())
        }
        4 => {
            let mut octets = [0_u8; 16];
            read_by(stream, &mut octets, deadline)?;
            Some(IpAddr::from(octets).to_string())
        }
        3 => {
            let mut length = [0_u8; 1];
            read_by(stream, &mut length, deadline)?;
            let mut name = vec![0_u8; usize::from(length[0])];
            read_by(stream, &mut name, deadline)?;
            Some(domain(&name))
        }
        _ => None,
    };
    let Some(host) = host else {
        return Ok(Asked::Refuse(reply::ADDRESS_NOT_SUPPORTED));
    };
    let mut port = [0_u8; 2];
    read_by(stream, &mut port, deadline)?;
    let port = u16::from_be_bytes(port);
    if request[1] != CONNECT {
        return Ok(Asked::Refuse(reply::COMMAND_NOT_SUPPORTED));
    }
    if host.is_empty() || port == 0 {
        return Ok(Asked::Refuse(reply::FAILURE));
    }
    Ok(Asked::Connect(Target::Tcp { host, port }))
}

/// A domain name as the host takes it: lowercase, and only the characters a host name has. Anything else is the empty text,
/// which is refused, so a name can never be read as another kind of target.
fn domain(name: &[u8]) -> String {
    let valid = !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'));
    if valid {
        String::from_utf8_lossy(name).to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// The reply to a client: `code` and the address the host's connection reached (`0.0.0.0:0` when it did not say).
fn answer(code: u8, bound: Option<SocketAddr>) -> Vec<u8> {
    let bound = bound.unwrap_or_else(|| SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0));
    let mut reply = vec![VERSION, code, 0];
    match bound.ip() {
        IpAddr::V4(address) => {
            reply.push(1);
            reply.extend_from_slice(&address.octets());
        }
        IpAddr::V6(address) => {
            reply.push(4);
            reply.extend_from_slice(&address.octets());
        }
    }
    reply.extend_from_slice(&bound.port().to_be_bytes());
    reply
}

/// Serves one client: the handshake within `settings.socks_timeout`, a connect, the reply, and the pump.
fn client(
    link: &dyn Open,
    mut stream: TcpStream,
    settings: &ForwardSettings,
    report: &(dyn Fn(&Error) + Sync),
) {
    // An accepted socket may inherit the listener's non-blocking mode; the handshake reads and writes with timeouts.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(settings.socks_timeout));
    let deadline = Instant::now() + settings.socks_timeout;
    let asked = match handshake(&mut stream, deadline) {
        Ok(asked) => asked,
        Err(error) => {
            report(&Error::Link(format!("a SOCKS5 client: {}", error.kind())));
            return;
        }
    };
    let target = match asked {
        Asked::Connect(target) => target,
        Asked::Refuse(code) => {
            let _ = stream.write_all(&answer(code, None));
            return;
        }
    };
    match connect(link, &target, settings) {
        Ok(tunnel) => {
            let bound = tunnel.address().and_then(|text| text.parse().ok());
            if stream
                .write_all(&answer(reply::SUCCEEDED, bound))
                .and_then(|()| stream.set_read_timeout(None))
                .is_err()
            {
                return;
            }
            tunnel.pump_tcp(&stream);
        }
        Err(error) => {
            let _ = stream.write_all(&answer(reply_code(&error), None));
            report(&error);
        }
    }
}

/// Serves SOCKS5 on `listener` until `stop` is set or the link ends: each client's CONNECT is a [`connect`] to its target, and
/// a [`Tunnel::pump_tcp`](crate::forward::Tunnel::pump_tcp) once the host answered. A client that does not finish its
/// handshake within [`ForwardSettings::socks_timeout`] is closed. One past [`ForwardSettings::max_connections`] is closed at
/// once. A failure goes to `report`.
///
/// # Errors
///
/// The listener's error, or an `InvalidInput` one for settings with a zero bound.
pub fn serve(
    link: &dyn Open,
    listener: &TcpListener,
    settings: &ForwardSettings,
    stop: &AtomicBool,
    report: &(dyn Fn(&Error) + Sync),
) -> io::Result<Ended> {
    settings
        .options
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if settings.socks_timeout == Duration::ZERO {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the SOCKS5 handshake timeout is not zero",
        ));
    }
    listener.set_nonblocking(true)?;
    let live = AtomicUsize::new(0);
    thread::scope(|scope| {
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(Ended::Stopped);
            }
            if !link.is_up() {
                return Ok(Ended::LinkLost);
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if live.load(Ordering::SeqCst) >= settings.max_connections.max(1) {
                        continue;
                    }
                    live.fetch_add(1, Ordering::SeqCst);
                    let live = &live;
                    scope.spawn(move || {
                        client(link, stream, settings, report);
                        live.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(settings.options.listen_poll);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => {}
                Err(error) => return Err(error),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_lowercase_and_only_a_host_name() {
        assert_eq!(domain(b"Example.COM"), "example.com");
        assert_eq!(domain(b"unix:/run/x.sock"), "");
        assert_eq!(domain(b""), "");
        assert_eq!(domain(b"a b"), "");
    }

    #[test]
    fn an_answer_carries_the_address_in_its_own_family() {
        assert_eq!(
            answer(0, Some("192.0.2.3:258".parse().unwrap())),
            [5, 0, 0, 1, 192, 0, 2, 3, 1, 2]
        );
        let six = answer(0, Some("[::1]:80".parse().unwrap()));
        assert_eq!(six.len(), 4 + 16 + 2);
        assert_eq!(&six[..4], &[5, 0, 0, 4]);
        assert_eq!(answer(5, None), [5, 5, 0, 1, 0, 0, 0, 0, 0, 0]);
    }
}
