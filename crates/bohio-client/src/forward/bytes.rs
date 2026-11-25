//! A forwarded stream, or a program's standard streams, as the byte stream a link can run over (`ssh -J`).
//!
//! [`Bytes`] turns what a previous host carries (a [`Tunnel`](super::Tunnel) to a TCP socket, a Unix socket or a pipe there,
//! or the standard streams of a program the host runs) into `Read + Write + Split`, which is all
//! [`link::stream`](crate::link::stream) needs to run a whole link over it. The jump host moves sealed bytes and nothing else.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cabuya::forward::Options;
use cabuya::mux::LinkStream;

use coqui_transport::{Failure, Halves, Interrupt, Split};

use super::{ForwardSettings, Tunnel, ipc};
use crate::Error;
use crate::exec::{Event, Exec};

/// What a carrier gave in one wait.
enum Piece {
    /// Bytes of the stream.
    Data(Vec<u8>),
    /// The far end ended its direction in order.
    End,
    /// Nothing yet.
    Nothing,
}

/// What carries the bytes: a forwarded stream or a program on the previous host.
trait Carrier: Send + Sync {
    /// The next bytes, waiting up to `wait`.
    fn next(&self, wait: Duration) -> io::Result<Piece>;
    /// Sends all of `bytes`, waiting for room.
    fn send(&self, bytes: &[u8]) -> io::Result<()>;
    /// Ends this side's direction.
    fn finish(&self);
    /// Ends both directions at once.
    fn close(&self);
}

fn other(error: &Error) -> io::Error {
    io::Error::other(error.to_string())
}

/// A tunnel's stream.
struct Streamed {
    stream: Box<dyn LinkStream>,
    options: Options,
    never: AtomicBool,
}

impl Carrier for Streamed {
    fn next(&self, wait: Duration) -> io::Result<Piece> {
        match self.stream.receive(Some(wait)) {
            Ok(Some(bytes)) => Ok(Piece::Data(bytes)),
            Ok(None) if self.stream.finished_by_peer() => Ok(Piece::End),
            Ok(None) => Err(io::Error::from(io::ErrorKind::ConnectionReset)),
            Err(Failure::Timeout) => Ok(Piece::Nothing),
            Err(failure) => Err(io::Error::other(format!("{failure:?}"))),
        }
    }

    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        for piece in bytes.chunks(self.options.chunk.max(1)) {
            if !ipc::send_all(&*self.stream, piece, &self.options, &self.never) {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
        }
        Ok(())
    }

    fn finish(&self) {
        let _ = self.stream.finish();
    }

    fn close(&self) {
        self.stream.close();
    }
}

/// A program on the previous host: its standard output is what is read, its standard input what is written.
struct Program {
    exec: Exec,
}

impl Carrier for Program {
    fn next(&self, wait: Duration) -> io::Result<Piece> {
        match self.exec.next(Some(wait)) {
            Ok(Some(Event::Stdout(bytes))) => Ok(Piece::Data(bytes)),
            Ok(Some(Event::Exit(_)) | None) => Ok(Piece::End),
            Ok(Some(_)) | Err(Error::Timeout) => Ok(Piece::Nothing),
            Err(error) => Err(other(&error)),
        }
    }

    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        self.exec.write_stdin(bytes).map_err(|error| other(&error))
    }

    fn finish(&self) {
        let _ = self.exec.finish_stdin();
    }

    fn close(&self) {
        self.exec.close();
    }
}

/// What both halves hold: the carrier, which is closed when the last of them is gone.
struct Shared {
    carrier: Box<dyn Carrier>,
    watch: Duration,
    /// Set by [`Interrupt`]: a blocked read ends.
    interrupted: AtomicBool,
    /// Set when the writing half is dropped: the link above is over, so a read that finds nothing ends.
    writer_gone: AtomicBool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.carrier.close();
    }
}

/// A byte stream through a previous host: what [`Connection::through`](crate::connect::Connection::through) runs a link over.
pub struct Bytes {
    shared: Arc<Shared>,
}

impl core::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bytes").finish_non_exhaustive()
    }
}

impl Bytes {
    fn new(carrier: Box<dyn Carrier>, watch: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                carrier,
                watch,
                interrupted: AtomicBool::new(false),
                writer_gone: AtomicBool::new(false),
            }),
        }
    }

    /// The bytes of a connected `tunnel`.
    #[must_use]
    pub fn tunnel(tunnel: Tunnel, settings: &ForwardSettings) -> Self {
        let options = tunnel.options;
        Self::new(
            Box::new(Streamed {
                stream: tunnel.into_stream(),
                options,
                never: AtomicBool::new(false),
            }),
            settings.options.watch,
        )
    }

    /// The standard streams of `exec`, a program started with its standard input open (`Stdin::Open`): its standard output
    /// is read, its standard input written, and its standard error dropped. The program is ended when the stream is.
    #[must_use]
    pub fn program(exec: Exec, settings: &ForwardSettings) -> Self {
        Self::new(Box::new(Program { exec }), settings.options.watch)
    }
}

/// The reading half of [`Bytes`].
pub struct BytesReader {
    shared: Arc<Shared>,
    pending: Vec<u8>,
    at: usize,
}

/// The writing half of [`Bytes`]; dropping it ends this side's direction.
pub struct BytesWriter {
    shared: Arc<Shared>,
}

impl Read for BytesReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            if self.at < self.pending.len() {
                let count = buffer.len().min(self.pending.len() - self.at);
                buffer[..count].copy_from_slice(&self.pending[self.at..self.at + count]);
                self.at += count;
                return Ok(count);
            }
            match self.shared.carrier.next(self.shared.watch)? {
                Piece::Data(bytes) => {
                    self.pending = bytes;
                    self.at = 0;
                }
                Piece::End => return Ok(0),
                Piece::Nothing => {
                    if self.shared.interrupted.load(Ordering::SeqCst)
                        || self.shared.writer_gone.load(Ordering::SeqCst)
                    {
                        self.shared.carrier.close();
                        return Ok(0);
                    }
                }
            }
        }
    }
}

impl Write for BytesWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.shared.carrier.send(buffer)?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for BytesWriter {
    fn drop(&mut self) {
        self.shared.writer_gone.store(true, Ordering::SeqCst);
        self.shared.carrier.finish();
    }
}

impl Split for Bytes {
    type Reader = BytesReader;
    type Writer = BytesWriter;

    fn split(self) -> io::Result<Halves<BytesReader, BytesWriter>> {
        let interrupt = {
            let shared = Arc::clone(&self.shared);
            Interrupt::new(move || {
                shared.interrupted.store(true, Ordering::SeqCst);
                shared.carrier.close();
            })
        };
        Ok(Halves::new(
            BytesReader {
                shared: Arc::clone(&self.shared),
                pending: Vec::new(),
                at: 0,
            },
            BytesWriter {
                shared: self.shared,
            },
        )
        .interrupt(interrupt))
    }
}
