// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The host's X display as the source's desktop: each connection of the X backend is a channel to the display's socket
//! on the host, joined to a connection on the loopback the backend reads and writes.

use std::io::{Error, ErrorKind, Result};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bohio_x11::{Connector, X11Desktop, X11Settings};

use crate::command::quote;
use crate::forward::PROTOCOL;
use crate::link::{Link, lock, within};
use crate::session::{Open, Session};

type Cookie = Option<(Vec<u8>, Vec<u8>)>;

/// How the X backend reaches the host's X server.
#[derive(Debug)]
struct Channels {
    link: Arc<Link>,
    display: u32,
    /// The host's cookie, asked once (`Some(None)`: the host has none to give).
    cookie: Mutex<Option<Cookie>>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Link")
            .field("target", &self.target())
            .finish()
    }
}

/// The desktop of `link`'s host display `display`.
pub(crate) fn desktop(link: &Arc<Link>, display: u32) -> X11Desktop {
    let mut settings = X11Settings::default();
    settings.display = Some(format!("{}:{display}", link.settings.host));
    // A display of another machine has no session of this node's to ask about, and nothing says whether it is locked:
    // the engine's `input_when_lock_unknown` decides whether input goes to it.
    settings.lock_unknown = true;
    settings.connector = Some(Arc::new(Channels {
        link: Arc::clone(link),
        display,
        cookie: Mutex::new(None),
    }));
    X11Desktop::new(settings)
}

impl Channels {
    /// `MIT-MAGIC-COOKIE-1` and the cookie the host's `xauth` holds for the display, when it has one.
    fn cookie(&self) -> Cookie {
        let mut held = lock(&self.cookie);
        if held.is_none() {
            *held = Some(self.ask());
        }
        held.clone().flatten()
    }

    fn ask(&self) -> Cookie {
        let settings = &self.link.settings;
        let line = format!(
            "exec {} list {}",
            quote(&settings.xauth),
            quote(&format!(":{}", self.display))
        );
        let answer = self.run(line)?;
        // `host/unix:0  MIT-MAGIC-COOKIE-1  <hex>`
        answer.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            let (_, name, data) = (words.next()?, words.next()?, words.next()?);
            (name == PROTOCOL).then_some(())?;
            Some((name.as_bytes().to_vec(), unhex(data)?))
        })
    }

    /// The standard output of `command` on the host, when it ran and succeeded.
    fn run(&self, command: String) -> Option<String> {
        let link = &self.link;
        let session = link
            .rt
            .block_on(Session::start(
                link,
                Open {
                    pty: None,
                    command: Some(command),
                    stdin: false,
                },
            ))
            .ok()?;
        let deadline = Instant::now() + link.settings.operation_timeout;
        let mut out = Vec::new();
        let mut chunk = [0_u8; 4096];
        while out.len() < link.settings.x11_answer_max && Instant::now() < deadline {
            match session.read(1, &mut chunk, 200).ok()? {
                -1 => break,
                0 => {}
                count => out.extend_from_slice(&chunk[..usize::try_from(count).ok()?]),
            }
        }
        let exit = session.wait(1000)?;
        (exit.code == Some(0)).then(|| String::from_utf8_lossy(&out).into_owned())
    }

    /// A stream joined to a channel to the display's socket on the host.
    fn open(&self) -> Result<TcpStream> {
        let link = &self.link;
        let path = format!("{}/X{}", link.settings.x11_socket_dir, self.display);
        let other = |what: String| Error::new(ErrorKind::ConnectionRefused, what);
        let channel = within(link, link.settings.operation_timeout, async {
            let conn = link.conn().await?;
            conn.handle
                .channel_open_direct_streamlocal(path.as_str())
                .await
                .map_err(|error| bohio_api::unavailable(&error.to_string()))
        })
        .map_err(|error| other(error.message().to_string()))?
        .map_err(|error| {
            other(format!(
                "{}: the host did not open its X socket {path}: {}",
                link.target(),
                error.message()
            ))
        })?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let client = TcpStream::connect(listener.local_addr()?)?;
        let ours = client.local_addr()?;
        // Another program on this machine could reach the listener first; only the connection made here is bridged.
        let mut joined = None;
        for _ in 0..self.link.settings.x11_max_strangers {
            let (stream, from) = listener.accept()?;
            if from == ours {
                joined = Some(stream);
                break;
            }
        }
        let accepted =
            joined.ok_or_else(|| other("the loopback was reached by others".to_owned()))?;
        accepted.set_nonblocking(true)?;
        let handle = link.rt.handle();
        let _guard = handle.enter();
        let local = tokio::net::TcpStream::from_std(accepted)?;
        handle.spawn(async move {
            let (mut local, mut remote) = (local, channel.into_stream());
            let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
        });
        client.set_nodelay(true)?;
        Ok(client)
    }
}

impl Connector for Channels {
    fn connect(&self) -> Result<bohio_x11::Wire> {
        let stream = self.open().inspect_err(|_| {
            // The host's cookie may have changed with its session: ask again at the next connection.
            *lock(&self.cookie) = None;
        })?;
        let (auth_name, auth_data) = self.cookie().unwrap_or_default();
        Ok(bohio_x11::Wire::new(stream.into(), auth_name, auth_data))
    }
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() & 1 == 1 || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::unhex;

    #[test]
    fn a_cookie_is_hex() {
        assert_eq!(unhex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex(""), Some(Vec::new()));
    }
}
