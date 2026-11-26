//! Running one process on a host: `bohio-exec/1`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use bohio_proto::compress::{Codec, Compression};
use bohio_proto::exec::{Exit, Input, Output, Request, Signal, Signalled};
use bohio_proto::protocol;
use bohio_proto::session::{Attach, Attached, Kind};
use bohio_transfer::compress::{Bounds, Caps, Decoder, Encoder, Piece};
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
    /// The host's answer to a signal: whether it was delivered.
    Signalled(Signalled),
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
    /// What the host reported it uses for output and accepts for standard input, once `started` arrived.
    used: Mutex<(Option<Compression>, Option<Compression>)>,
    /// The decoders of standard output and standard error, made when `started` says output is compressed.
    decoders: Mutex<(Option<Decoder>, Option<Decoder>)>,
    /// The encoder of standard input, when the request declared it compressed.
    encoder: Mutex<Option<Encoder>>,
    window_log_max: u32,
    max_expansion: u32,
    /// The session's id, once `started` said the request made one (or the stream attached to one).
    session: Mutex<Option<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        Self::start_with(
            link,
            request,
            &Caps {
                window_log_max: 27,
                ..Caps::default()
            },
        )
    }

    /// As [`Exec::start`], compressing standard input within `caps` and refusing output that asks for a
    /// window above `caps.window_log_max` (`start` accepts 128 MiB; a host's own `window_log_max` sets
    /// what it sends). The request's `stdin_compression` names the algorithm, never `auto`.
    ///
    /// # Errors
    ///
    /// As [`Exec::start`]; [`Error::Compression`] when the algorithm of standard input is `auto` or this
    /// build lacks it.
    pub fn start_with(link: &dyn Open, request: &Request, caps: &Caps) -> Result<Self, Error> {
        let encoder = match &request.stdin_compression {
            Some(wanted) if wanted.algorithm.compresses() => Some(
                Encoder::new(wanted, caps)
                    .map_err(|error| Error::Compression(error.to_string()))?,
            ),
            Some(wanted) if wanted.algorithm == Codec::Auto => {
                return Err(Error::Compression(
                    "name the algorithm to send standard input with: auto is for what the host sends"
                        .to_owned(),
                ));
            }
            _ => None,
        };
        let metadata = request.to_metadata()?;
        Ok(Self {
            channel: link.open(protocol::EXEC, &metadata)?,
            used: Mutex::new((None, None)),
            decoders: Mutex::new((None, None)),
            encoder: Mutex::new(encoder),
            window_log_max: caps.window_log_max,
            max_expansion: crate::fs::DEFAULT_MAX_EXPANSION,
            session: Mutex::new(None),
        })
    }

    /// How many times the compressed bytes received the output may grow to, once it passes 1 MiB (the output of a
    /// process that prints a long run of one byte compresses a thousand to one and more); `0` turns the check off.
    /// Default [`crate::fs::DEFAULT_MAX_EXPANSION`]. Set it before the first event is read.
    #[must_use]
    pub const fn max_expansion(mut self, times: u32) -> Self {
        self.max_expansion = times;
        self
    }

    /// Attaches to the detached session `session` of an exec on `link` (`bohio-attach/1`), reading its output from the
    /// offset `from` (`0` for all that is still kept, `None` for only what comes next). The stream then gives the same
    /// events as the one that started it, from there on; no `Started` event comes, and standard input and signals go to the
    /// process as they did. A session's streams are not compressed.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host refused (`not_found` for a session this peer does not hold, `denied`, ...),
    /// [`Error::Protocol`] when the session is a shell, else whatever `link` reports.
    pub fn attach(
        link: &dyn Open,
        session: &str,
        from: Option<u64>,
    ) -> Result<(Self, Attached), Error> {
        let (channel, attached) = attach_channel(link, session, from)?;
        if attached.kind != Kind::Exec {
            channel.close();
            return Err(Error::Protocol(bohio_proto::Error::Decode));
        }
        Ok((Self::attached_to(channel, session), attached))
    }

    /// An exec on a channel that attached to the session `session`.
    pub(crate) fn attached_to(channel: Box<dyn Channel>, session: &str) -> Self {
        Self {
            channel,
            used: Mutex::new((None, None)),
            decoders: Mutex::new((None, None)),
            encoder: Mutex::new(None),
            window_log_max: 27,
            max_expansion: crate::fs::DEFAULT_MAX_EXPANSION,
            session: Mutex::new(Some(session.to_owned())),
        }
    }

    /// The session's id, when the request asked for a session and `started` has arrived, or this stream attached to one.
    pub fn session(&self) -> Option<String> {
        lock(&self.session).clone()
    }

    /// What the host reported it uses for output and accepts for standard input (the level it clamped
    /// to), known once the `Started` event arrived; `None` for a raw stream.
    pub fn compression(&self) -> (Option<Compression>, Option<Compression>) {
        lock(&self.used).clone()
    }

    fn decode(&self, error: &bohio_transfer::compress::DecodeError) -> Error {
        use bohio_transfer::compress::DecodeError;
        match error {
            DecodeError::Unsupported(_) | DecodeError::Setup(_) => {
                Error::Compression(error.to_string())
            }
            _ => Error::Protocol(bohio_proto::Error::Decode),
        }
    }

    /// Decodes a piece of standard output (`stderr` false) or standard error; `None` when it yields nothing yet.
    fn decoded(&self, piece: &[u8], stderr: bool) -> Result<Option<Vec<u8>>, Error> {
        let mut decoders = lock(&self.decoders);
        let slot = if stderr {
            &mut decoders.1
        } else {
            &mut decoders.0
        };
        let decoder = slot
            .as_mut()
            .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
        let mut out = Vec::new();
        decoder
            .feed(piece, &mut out)
            .map_err(|error| self.decode(&error))?;
        Ok((!out.is_empty()).then_some(out))
    }

    /// A raw frame ends the compression context of its stream.
    fn raw(&self, stderr: bool) {
        let mut decoders = lock(&self.decoders);
        let slot = if stderr {
            &mut decoders.1
        } else {
            &mut decoders.0
        };
        if let Some(decoder) = slot.as_mut() {
            decoder.reset();
        }
    }

    fn start_decoders(&self, used: &Compression) -> Result<(), Error> {
        if !used.algorithm.compresses() {
            return Ok(());
        }
        let bounds = Bounds {
            window_log_max: self.window_log_max,
            max_output: None,
            max_expansion: self.max_expansion,
        };
        let make = || Decoder::new(used, &bounds).map_err(|error| self.decode(&error));
        *lock(&self.decoders) = (Some(make()?), Some(make()?));
        Ok(())
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
                    if let Some(used) = &started.output_compression {
                        self.start_decoders(used)?;
                    }
                    *lock(&self.used) = (started.output_compression, started.stdin_compression);
                    if started.session.is_some() {
                        *lock(&self.session) = started.session;
                    }
                    return Ok(Some(Event::Started { pid: started.pid }));
                }
                Ok(Output::Stdout(bytes)) => {
                    self.raw(false);
                    return Ok(Some(Event::Stdout(bytes)));
                }
                Ok(Output::Stderr(bytes)) => {
                    self.raw(true);
                    return Ok(Some(Event::Stderr(bytes)));
                }
                Ok(Output::StdoutCompressed(piece)) => {
                    if let Some(bytes) = self.decoded(&piece, false)? {
                        return Ok(Some(Event::Stdout(bytes)));
                    }
                }
                Ok(Output::StderrCompressed(piece)) => {
                    if let Some(bytes) = self.decoded(&piece, true)? {
                        return Ok(Some(Event::Stderr(bytes)));
                    }
                }
                Ok(Output::Signalled(answer)) => return Ok(Some(Event::Signalled(answer))),
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
            let compressed = match lock(&self.encoder).as_mut() {
                Some(encoder) => match encoder
                    .frame(piece)
                    .map_err(|error| Error::Compression(error.to_string()))?
                {
                    Piece::Compressed(compressed) => Some(compressed),
                    Piece::Raw => None,
                },
                None => None,
            };
            let frame =
                compressed.map_or_else(|| Input::Stdin(piece.to_vec()), Input::StdinCompressed);
            self.send(&frame.encode())?;
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

    /// Asks the host to deliver `signal` to the process (its process group on Unix). The host answers with an
    /// [`Event::Signalled`] saying whether it did.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the channel failed.
    pub fn signal(&self, signal: Signal) -> Result<(), Error> {
        self.send(&Input::Signal(signal).encode())
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
                Some(Event::Signalled(_)) => {}
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

/// Opens `bohio-attach/1` and reads its first frame, `attached`.
pub(crate) fn attach_channel(
    link: &dyn Open,
    session: &str,
    from: Option<u64>,
) -> Result<(Box<dyn Channel>, Attached), Error> {
    let request = Attach {
        session: session.to_owned(),
        from,
    };
    let channel = link.open(protocol::ATTACH, &request.to_metadata()?)?;
    loop {
        let Some(frame) = channel.receive(None)? else {
            return Err(Error::Closed);
        };
        match Attached::decode(&frame) {
            Ok(attached) => return Ok((channel, attached)),
            Err(bohio_proto::Error::Tag(_)) => {
                if let Ok(Output::Error(body)) = Output::decode(&frame) {
                    return Err(Error::Remote(body));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

impl Drop for Exec {
    fn drop(&mut self) {
        self.channel.close();
    }
}
