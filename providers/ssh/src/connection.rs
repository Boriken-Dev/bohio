// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The connection as a library: what a program built on this crate does with the host besides the kinds. It shares
//! the source's connection, so a login is made once.

use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Instant;

use bohio_api::guatiao::Status;
use bohio_api::guatiao::library::ProviderError;
use bohio_api::model::TerminalExit;

use crate::link::{Link, Rt, lock, refuse, within};
use crate::reverse::{self, ReverseForward};
use crate::session::{Open, Session};
use crate::settings::SshSettings;

/// How long one read of a command's output waits before the deadline is looked at again, in milliseconds.
const READ_SLICE_MS: u64 = 200;

/// What a command run through [`Connection::run`] printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Captured {
    /// Standard output, at most the `max_output` asked for.
    pub stdout: Vec<u8>,
    /// Whether the output was longer than `max_output` and the rest was dropped.
    pub truncated: bool,
    /// How the command ended; `None` when it did not end within the `operation_timeout` plus `exit_grace` that follow
    /// its last output.
    pub exit: Option<TerminalExit>,
}

/// The system a host runs, from `uname -sm`, named as `std::env::consts` names them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Platform {
    /// `linux`, `macos`, `freebsd`, ...; `None` when the host did not say.
    pub os: Option<String>,
    /// `x86_64`, `aarch64`, `x86`, ...; `None` when the host did not say.
    pub arch: Option<String>,
}

/// One host, reached with the settings of an ssh source. Cloning shares the connection and its limits.
#[derive(Clone)]
pub struct Connection {
    link: Arc<Link>,
}

impl core::fmt::Debug for Connection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Connection")
            .field("target", &self.link.target())
            .finish()
    }
}

impl Connection {
    /// Checks `settings` and starts the runtime that drives the connection; nothing touches the network or a key file
    /// until the first call that needs the host.
    ///
    /// # Errors
    /// A bad value, naming the setting.
    pub fn new(settings: &SshSettings) -> Result<Self, ProviderError> {
        let read = settings.read()?;
        let rt = Rt::new(read.worker_threads)?;
        Ok(Self::of(Link::new(read, rt)))
    }

    pub(crate) fn of(link: Arc<Link>) -> Self {
        Self { link }
    }

    /// `HOST:PORT`, as the messages name the machine.
    #[must_use]
    pub fn target(&self) -> String {
        self.link.target()
    }

    /// The key the host presented at first contact, when `host_key_check = "tofu"` accepted one: what to pin with
    /// `host_key` afterwards. `None` for any other policy, or before the first connection.
    #[must_use]
    pub fn trusted_host_key(&self) -> Option<String> {
        lock(&self.link.keys.trusted).clone()
    }

    /// Runs `command` (one line for the account's login shell: the caller quotes it) on an exec channel and returns
    /// at most `max_output` bytes of what it wrote to standard output, once it has ended. Standard error is left
    /// unread. The settings' `operation_timeout` bounds the wait for the output to end and `exit_grace` the wait for
    /// the exit after it.
    ///
    /// # Errors
    /// `GONE` when the host cannot be reached, `BUSY` when `max_channels` sessions are open.
    pub fn run(&self, command: &str, max_output: usize) -> Result<Captured, ProviderError> {
        let link = &self.link;
        let session = link.rt.block_on(Session::start(
            link,
            Open {
                pty: None,
                command: Some(command.to_owned()),
                stdin: false,
            },
        ))?;
        let deadline = Instant::now() + link.settings.operation_timeout;
        let (mut out, mut chunk, mut truncated) = (Vec::new(), [0_u8; 4096], false);
        while Instant::now() < deadline {
            match session.read(1, &mut chunk, READ_SLICE_MS)? {
                -1 => break,
                0 => {}
                count => {
                    let count = usize::try_from(count).unwrap_or(0);
                    let room = max_output.saturating_sub(out.len());
                    out.extend_from_slice(&chunk[..count.min(room)]);
                    truncated |= count > room;
                }
            }
        }
        let grace = u64::try_from(link.settings.exit_grace.as_millis()).unwrap_or(u64::MAX);
        let exit = session.wait(grace);
        if exit.is_none() {
            let _ = session.kill();
        }
        Ok(Captured {
            stdout: out,
            truncated,
            exit,
        })
    }

    /// The system the host runs: `uname -sm` on an exec channel.
    ///
    /// # Errors
    /// As [`Connection::run`]. A host whose `uname` fails answers a `Platform` with nothing in it.
    pub fn platform(&self) -> Result<Platform, ProviderError> {
        let found = self.run("uname -sm", 1024)?;
        if found.exit.as_ref().and_then(|exit| exit.code) != Some(0) {
            return Ok(Platform::default());
        }
        let (os, arch) = crate::command::parse_uname(&String::from_utf8_lossy(&found.stdout));
        Ok(Platform { os, arch })
    }

    /// A stream joined to a channel to the unix socket `path` on the host (`direct-streamlocal@openssh.com`). The
    /// channel does not count against `max_channels`, which counts sessions.
    ///
    /// The stream is one end of a connection on this machine's loopback, whose other end the connection's runtime
    /// relays to the channel, so a caller that needs a socket (a protocol library that reads one) has it. Only the
    /// connection this call made is joined: others that reach the listener first are dropped, at most
    /// `max_strangers` of them before the call gives up.
    ///
    /// # Errors
    /// `GONE` when the host is unreachable or does not open the socket; the loopback being reached by `max_strangers`
    /// other connections is an error too. With `client = "openssh"` the answer is `unsupported`.
    pub fn open_socket(
        &self,
        path: &str,
        max_strangers: usize,
    ) -> Result<TcpStream, ProviderError> {
        let link = &self.link;
        if link.settings.openssh.is_some() {
            return Err(bohio_api::unsupported(
                "a channel to a socket needs the one connection this provider keeps: client = \"openssh\" starts a process for each session",
            ));
        }
        let gone = |what: String| refuse(Status::GUATIAO_ERR_GONE, what);
        let channel = within(link, link.settings.operation_timeout, async {
            let conn = link.conn().await?;
            conn.handle
                .channel_open_direct_streamlocal(path)
                .await
                .map_err(|error| {
                    gone(format!(
                        "{}: the host did not open its socket {path}: {error}",
                        link.target()
                    ))
                })
        })??;
        let io = |error: std::io::Error| gone(format!("the loopback relay: {error}"));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(io)?;
        let client = TcpStream::connect(listener.local_addr().map_err(io)?).map_err(io)?;
        let ours = client.local_addr().map_err(io)?;
        let mut joined = None;
        for _ in 0..max_strangers {
            let (stream, from) = listener.accept().map_err(io)?;
            if from == ours {
                joined = Some(stream);
                break;
            }
        }
        let accepted =
            joined.ok_or_else(|| gone("the loopback was reached by others".to_owned()))?;
        accepted.set_nonblocking(true).map_err(io)?;
        let handle = link.rt.handle();
        let _guard = handle.enter();
        let local = tokio::net::TcpStream::from_std(accepted).map_err(io)?;
        handle.spawn(async move {
            let (mut local, mut remote) = (local, channel.into_stream());
            let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
        });
        client.set_nodelay(true).map_err(io)?;
        Ok(client)
    }

    /// Asks the host to listen on `remote_host:remote_port` (port `0`: the host chooses) and carries each connection
    /// to `target` (`HOST:PORT`, on this machine's side). The server must allow TCP forwarding.
    ///
    /// # Errors
    /// `INTERNAL` when the host does not accept the forward, `GONE` when it is unreachable; with
    /// `client = "openssh"` the answer is `unsupported`.
    pub fn reverse_forward(
        &self,
        remote_host: &str,
        remote_port: u16,
        target: &str,
    ) -> Result<ReverseForward, ProviderError> {
        reverse::open(&self.link, remote_host, remote_port, target)
    }
}
