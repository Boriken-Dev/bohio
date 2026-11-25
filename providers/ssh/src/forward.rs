// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! X11 forwarding: windows of programs this source starts open on the node's own display.
//!
//! The host's sshd listens for the programs' X connections and opens one `x11` channel for each. The first message of
//! an X connection carries an authorization cookie: the one this source gave the host is checked and replaced by the one
//! the node's display wants, then the bytes are relayed both ways.

use std::sync::Arc;

use russh::Channel;
use russh::client::Msg;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

use crate::settings::{Settings, display_number};

/// The authorization protocol of every cookie here.
pub(crate) const PROTOCOL: &str = "MIT-MAGIC-COOKIE-1";

/// The bytes of the X connection request before its two variable parts.
const HEADER: usize = 12;

/// What one source needs to forward: the cookie it gave the host, where the node's display is, and how many may be open.
pub(crate) struct Forward {
    fake: [u8; 16],
    display: Option<String>,
    pub(crate) slots: Arc<Semaphore>,
}

impl Forward {
    /// `None` when the source does not forward.
    pub(crate) fn new(settings: &Settings) -> Option<Arc<Self>> {
        if !settings.x11_forward {
            return None;
        }
        let mut fake = [0_u8; 16];
        // A cookie nobody can guess is what keeps another program on the host out of the node's display.
        getrandom::fill(&mut fake).ok()?;
        Some(Arc::new(Self {
            fake,
            display: settings.x11_local_display.clone(),
            slots: Arc::new(Semaphore::new(settings.x11_max_connections)),
        }))
    }

    /// The cookie, as the hex the `x11-req` carries.
    pub(crate) fn cookie_hex(&self) -> String {
        hex(&self.fake)
    }

    /// Relays one X connection of the host to the node's display until either side ends.
    pub(crate) async fn relay(&self, channel: Channel<Msg>) -> Result<(), String> {
        let mut remote = channel.into_stream();
        let request = read_request(&mut remote).await?;
        if request.name != PROTOCOL.as_bytes() || !same(&request.data, &self.fake) {
            return Err("an X connection carried a cookie this source did not give".to_owned());
        }
        let display = self
            .display
            .clone()
            .or_else(|| {
                std::env::var("DISPLAY")
                    .ok()
                    .filter(|text| !text.is_empty())
            })
            .ok_or_else(|| "the node has no display to forward to (no DISPLAY)".to_owned())?;
        let target = Local::of(&display)?;
        let cookie = target.cookie();
        let mut local = target.connect().await?;
        local
            .write_all(&request.with_cookie(cookie.as_ref()))
            .await
            .map_err(|error| format!("the node's display: {error}"))?;
        tokio::io::copy_bidirectional(&mut remote, &mut local)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether two cookies are equal, taking the same time whatever the bytes are.
fn same(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0, |all, (a, b)| all | (a ^ b)) == 0
}

/// The first message of an X connection: the byte order, the protocol version and the authorization.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    big: bool,
    version: [u8; 4],
    pub(crate) name: Vec<u8>,
    pub(crate) data: Vec<u8>,
}

fn padded(len: usize) -> usize {
    len.div_ceil(4) * 4
}

async fn read_request<R: tokio::io::AsyncRead + Unpin>(from: &mut R) -> Result<Request, String> {
    let mut head = [0_u8; HEADER];
    from.read_exact(&mut head)
        .await
        .map_err(|error| format!("the host's X connection ended at its start: {error}"))?;
    let big = match head[0] {
        b'B' => true,
        b'l' => false,
        _ => return Err("an X connection began with neither byte order".to_owned()),
    };
    let number = |at: usize| {
        let pair = [head[at], head[at + 1]];
        usize::from(if big {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        })
    };
    let (name_len, data_len) = (number(6), number(8));
    let mut rest = vec![0_u8; padded(name_len) + padded(data_len)];
    from.read_exact(&mut rest)
        .await
        .map_err(|error| format!("the host's X connection ended in its request: {error}"))?;
    Ok(Request {
        big,
        version: [head[2], head[3], head[4], head[5]],
        name: rest[..name_len].to_vec(),
        data: rest[padded(name_len)..padded(name_len) + data_len].to_vec(),
    })
}

impl Request {
    /// The request again with `cookie` as its authorization (none when the display wants none).
    pub(crate) fn with_cookie(&self, cookie: Option<&(Vec<u8>, Vec<u8>)>) -> Vec<u8> {
        let (name, data) = cookie.map_or((&[][..], &[][..]), |(name, data)| (name, data));
        let short = |len: usize| {
            let len = u16::try_from(len).unwrap_or(u16::MAX);
            if self.big {
                len.to_be_bytes()
            } else {
                len.to_le_bytes()
            }
        };
        let mut out = Vec::with_capacity(HEADER + padded(name.len()) + padded(data.len()));
        out.push(if self.big { b'B' } else { b'l' });
        out.push(0);
        out.extend_from_slice(&self.version);
        out.extend_from_slice(&short(name.len()));
        out.extend_from_slice(&short(data.len()));
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name);
        out.resize(HEADER + padded(name.len()), 0);
        out.extend_from_slice(data);
        out.resize(HEADER + padded(name.len()) + padded(data.len()), 0);
        out
    }
}

/// The node's display, as a place to connect to.
enum Local {
    #[cfg(unix)]
    Socket(String, u16),
    Tcp(String, u16),
}

impl Local {
    fn of(display: &str) -> Result<Self, String> {
        let (host, rest) = display
            .rsplit_once(':')
            .ok_or_else(|| format!("`{display}` is not an X display (`:0`)"))?;
        let number = display_number("x11_local_display", &format!(":{rest}"))
            .map_err(|error| error.message().to_string())?;
        let number =
            u16::try_from(number).map_err(|_| "the display number is too large".to_owned())?;
        if host.is_empty() || host == "unix" {
            #[cfg(unix)]
            return Ok(Self::Socket(format!("/tmp/.X11-unix/X{number}"), number));
            #[cfg(not(unix))]
            return Err("this node has no X display: forwarding needs one (set x11_local_display to a display on another machine)".to_owned());
        }
        Ok(Self::Tcp(host.to_owned(), number))
    }

    /// The cookie the display wants of a connection from this account, from the account's own authority file.
    fn cookie(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        use x11rb::reexports::x11rb_protocol::xauth::{Family, get_auth};
        let number = match self {
            #[cfg(unix)]
            Self::Socket(_, number) => *number,
            Self::Tcp(_, number) => *number,
        };
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .ok()
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_default();
        get_auth(Family::LOCAL, host.trim().as_bytes(), number)
            .ok()
            .flatten()
    }

    async fn connect(&self) -> Result<Box<dyn Stream>, String> {
        match self {
            #[cfg(unix)]
            Self::Socket(path, _) => tokio::net::UnixStream::connect(path)
                .await
                .map(|stream| Box::new(stream) as Box<dyn Stream>)
                .map_err(|error| format!("the node's display {path}: {error}")),
            Self::Tcp(host, number) => {
                tokio::net::TcpStream::connect((host.as_str(), 6000 + number))
                    .await
                    .map(|stream| Box::new(stream) as Box<dyn Stream>)
                    .map_err(|error| format!("the node's display {host}:{number}: {error}"))
            }
        }
    }
}

/// Either kind of connection to the display.
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}

impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(big: bool, name: &[u8], data: &[u8]) -> Vec<u8> {
        Request {
            big,
            version: if big { [0, 11, 0, 0] } else { [11, 0, 0, 0] },
            name: Vec::new(),
            data: Vec::new(),
        }
        .with_cookie(Some(&(name.to_vec(), data.to_vec())))
    }

    #[tokio::test]
    async fn a_request_is_read_and_written_back_in_either_byte_order() {
        for big in [false, true] {
            let bytes = raw(big, PROTOCOL.as_bytes(), &[7; 16]);
            assert_eq!(bytes.len() % 4, 0);
            let request = read_request(&mut bytes.as_slice()).await.unwrap();
            assert_eq!(request.name, PROTOCOL.as_bytes());
            assert_eq!(request.data, vec![7; 16]);
            assert_eq!(
                request.with_cookie(Some(&(request.name.clone(), request.data.clone()))),
                bytes
            );
        }
    }

    #[tokio::test]
    async fn the_cookie_is_replaced_and_none_leaves_the_fields_empty() {
        let request = read_request(&mut raw(false, PROTOCOL.as_bytes(), &[1; 16]).as_slice())
            .await
            .unwrap();
        let swapped = request.with_cookie(Some(&(PROTOCOL.as_bytes().to_vec(), vec![9; 16])));
        let again = read_request(&mut swapped.as_slice()).await.unwrap();
        assert_eq!(again.data, vec![9; 16]);
        let bare = request.with_cookie(None);
        assert_eq!(bare.len(), HEADER);
        let again = read_request(&mut bare.as_slice()).await.unwrap();
        assert!(again.name.is_empty() && again.data.is_empty());
    }

    #[tokio::test]
    async fn a_request_that_is_not_x_is_refused() {
        let error = read_request(&mut [b'G'; 12].as_slice()).await.unwrap_err();
        assert!(error.contains("byte order"), "{error}");
    }

    #[test]
    fn cookies_compare_whole() {
        assert!(same(&[1, 2], &[1, 2]));
        assert!(!same(&[1, 2], &[1, 3]));
        assert!(!same(&[1, 2], &[1]));
    }

    #[test]
    fn a_display_is_a_socket_or_a_tcp_port() {
        assert!(Local::of("nonsense").is_err());
        assert!(matches!(Local::of("far:2"), Ok(Local::Tcp(host, 2)) if host == "far"));
        #[cfg(unix)]
        assert!(
            matches!(Local::of(":3.0"), Ok(Local::Socket(path, 3)) if path == "/tmp/.X11-unix/X3")
        );
    }
}
