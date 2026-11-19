//! An interactive shell on a host: `bohio-shell/1`.

use std::time::{Duration, Instant};

use bohio_proto::protocol;
use bohio_proto::shell::{Exit, Input, Output, Request, Size};
use coqui_transport::Channel;

use crate::{Error, Open};

/// One frame from the host after the shell started.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// Bytes the terminal produced: the shell's output as a terminal renders it.
    Data(Vec<u8>),
    /// The shell ended; the last event.
    Exit(Exit),
}

/// A shell running on the host. Every method takes `&self`, so one thread can type while another
/// reads. Dropping it closes the stream, which ends the shell's process tree.
pub struct Shell {
    channel: Box<dyn Channel>,
    pid: u32,
}

impl core::fmt::Debug for Shell {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shell")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl Shell {
    /// Opens a `bohio-shell/1` stream for `request` on `link` and waits up to `timeout` (`None`:
    /// without limit) for the host to start the shell.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host refused (`denied`, `not_found`, `busy`, ...),
    /// [`Error::Timeout`], [`Error::Closed`], or whatever `link` reports.
    pub fn start(
        link: &dyn Open,
        request: &Request,
        timeout: Option<Duration>,
    ) -> Result<Self, Error> {
        let channel = link.open(protocol::SHELL, &request.to_metadata()?)?;
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = channel.receive(left)? else {
                return Err(Error::Closed);
            };
            match Output::decode(&frame) {
                Ok(Output::Started(started)) => {
                    return Ok(Self {
                        channel,
                        pid: started.pid,
                    });
                }
                Ok(Output::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// The host's process id of the shell.
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// The next event, waiting up to `timeout` (`None`: without limit). `Ok(None)` once the host
    /// closed the stream in order. A frame this version does not know is skipped.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] for the host's error frame, [`Error::Timeout`] when nothing arrived (the
    /// session goes on), [`Error::Transport`] or [`Error::Protocol`] otherwise.
    pub fn next(&self, timeout: Option<Duration>) -> Result<Option<Event>, Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = self.channel.receive(left)? else {
                return Ok(None);
            };
            match Output::decode(&frame) {
                Ok(Output::Data(bytes)) => return Ok(Some(Event::Data(bytes))),
                Ok(Output::Exit(exit)) => return Ok(Some(Event::Exit(exit))),
                Ok(Output::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Types `bytes` at the terminal, split into messages the channel carries, waiting until it
    /// takes them. Enter is a carriage return (`\r`).
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the channel failed.
    pub fn write(&self, bytes: &[u8]) -> Result<(), Error> {
        let room = self
            .channel
            .capabilities()
            .max_send
            .map_or(bytes.len(), |max| max.saturating_sub(1))
            .max(1);
        for piece in bytes.chunks(room) {
            self.send(&Input::Data(piece.to_vec()).encode()?)?;
        }
        Ok(())
    }

    /// Tells the host the terminal is now `cols` by `rows`.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the channel failed.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), Error> {
        self.send(&Input::Resize(Size { cols, rows }).encode()?)
    }

    /// Asks the host to end the shell's process tree now.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the channel failed.
    pub fn kill(&self) -> Result<(), Error> {
        self.send(&Input::Kill.encode()?)
    }

    /// Reads output until `needle` has appeared in it, for at most `timeout` (`None`: without
    /// limit), and returns everything read. Bytes after the match in the same chunk are included.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] when `timeout` passed (the session goes on), [`Error::Closed`] when the
    /// shell ended or the stream closed first, [`Error::Remote`] for the host's error frame.
    pub fn read_until(&self, needle: &[u8], timeout: Option<Duration>) -> Result<Vec<u8>, Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let mut seen = Vec::new();
        loop {
            if !needle.is_empty() && seen.windows(needle.len()).any(|window| window == needle) {
                return Ok(seen);
            }
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            match self.next(left)? {
                Some(Event::Data(bytes)) => seen.extend_from_slice(&bytes),
                Some(Event::Exit(_)) | None => return Err(Error::Closed),
            }
        }
    }

    /// Reads until the shell ends, for at most `timeout`, and returns the output that remained
    /// and how it ended.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] when `timeout` passed, [`Error::Closed`] when the stream ended before
    /// the exit, [`Error::Remote`] for the host's error frame.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<(Vec<u8>, Exit), Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let mut output = Vec::new();
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            match self.next(left)? {
                Some(Event::Data(bytes)) => output.extend_from_slice(&bytes),
                Some(Event::Exit(exit)) => return Ok((output, exit)),
                None => return Err(Error::Closed),
            }
        }
    }

    /// Closes the stream; the host ends the shell's process tree if it still runs.
    pub fn close(&self) {
        self.channel.close();
    }

    fn send(&self, message: &[u8]) -> Result<(), Error> {
        self.channel.send_timeout(message, None)?;
        Ok(())
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.channel.close();
    }
}
