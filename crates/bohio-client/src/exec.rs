//! Running one process on a host: `bohio-exec/1`.

use std::time::{Duration, Instant};

use bohio_proto::exec::{Exit, Input, Output, Request};
use bohio_proto::protocol;
use coqui_transport::Channel;

use crate::{Error, Open};

/// One frame from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The process started with this process id on the host.
    Started {
        /// The host's process id.
        pid: u32,
    },
    /// Bytes written to standard output.
    Stdout(Vec<u8>),
    /// Bytes written to standard error.
    Stderr(Vec<u8>),
    /// The process ended; the last event.
    Exit(Exit),
}

/// Everything a finished process produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed {
    /// The host's process id, when the host reported it.
    pub pid: Option<u32>,
    /// All of standard output.
    pub stdout: Vec<u8>,
    /// All of standard error.
    pub stderr: Vec<u8>,
    /// How it ended.
    pub exit: Exit,
}

/// A process running on the host. Every method takes `&self`, so one thread can write standard
/// input while another reads events. Dropping it closes the stream, which ends the process.
pub struct Exec {
    channel: Box<dyn Channel>,
}

impl core::fmt::Debug for Exec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Exec").finish_non_exhaustive()
    }
}

impl Exec {
    /// Opens a `bohio-exec/1` stream for `request` on `link`.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when the request cannot be encoded, else whatever `link` reports.
    pub fn start(link: &dyn Open, request: &Request) -> Result<Self, Error> {
        let metadata = request.to_metadata()?;
        Ok(Self {
            channel: link.open(protocol::EXEC, &metadata)?,
        })
    }

    /// The next event, waiting up to `timeout` (`None`: without limit). `Ok(None)` once the
    /// host closed the stream in order. A frame this version does not know is skipped.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] for the host's error frame, [`Error::Timeout`] when nothing arrived (the
    /// stream goes on), [`Error::Transport`] or [`Error::Protocol`] otherwise.
    pub fn next(&self, timeout: Option<Duration>) -> Result<Option<Event>, Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = self.channel.receive(left)? else {
                return Ok(None);
            };
            match Output::decode(&frame) {
                Ok(Output::Started(started)) => {
                    return Ok(Some(Event::Started { pid: started.pid }));
                }
                Ok(Output::Stdout(bytes)) => return Ok(Some(Event::Stdout(bytes))),
                Ok(Output::Stderr(bytes)) => return Ok(Some(Event::Stderr(bytes))),
                Ok(Output::Exit(exit)) => return Ok(Some(Event::Exit(exit))),
                Ok(Output::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Sends standard input, split into messages the channel carries, waiting until it takes them.
    /// The request must have asked for `Stdin::Open`.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the channel failed.
    pub fn write_stdin(&self, bytes: &[u8]) -> Result<(), Error> {
        let room = self
            .channel
            .capabilities()
            .max_send
            .map_or(bytes.len(), |max| max.saturating_sub(1))
            .max(1);
        for piece in bytes.chunks(room) {
            self.send(&Input::Stdin(piece.to_vec()).encode())?;
        }
        Ok(())
    }

    /// Ends standard input: the process reads the end of its input.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the channel failed.
    pub fn finish_stdin(&self) -> Result<(), Error> {
        self.send(&Input::StdinEnd.encode())
    }

    /// Asks the host to end the process tree now.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the channel failed.
    pub fn kill(&self) -> Result<(), Error> {
        self.send(&Input::Kill.encode())
    }

    /// Reads events until the process ends, collecting its output, for at most `timeout`
    /// (`None`: without limit). The process keeps running if the wait times out.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] for the host's error frame, [`Error::Timeout`] when `timeout` passed,
    /// [`Error::Closed`] when the stream ended before the exit.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<Completed, Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let mut done = Completed {
            pid: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: Exit::default(),
        };
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            match self.next(left)? {
                Some(Event::Started { pid }) => done.pid = Some(pid),
                Some(Event::Stdout(bytes)) => done.stdout.extend_from_slice(&bytes),
                Some(Event::Stderr(bytes)) => done.stderr.extend_from_slice(&bytes),
                Some(Event::Exit(exit)) => {
                    done.exit = exit;
                    return Ok(done);
                }
                None => return Err(Error::Closed),
            }
        }
    }

    /// Closes the stream; the host ends the process tree if it still runs.
    pub fn close(&self) {
        self.channel.close();
    }

    fn send(&self, message: &[u8]) -> Result<(), Error> {
        self.channel.send_timeout(message, None)?;
        Ok(())
    }
}

impl Drop for Exec {
    fn drop(&mut self) {
        self.channel.close();
    }
}
