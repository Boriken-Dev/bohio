//! A Unix domain socket or a Windows named pipe on the far end of a forward, in either direction: two threads over a
//! [`coqui_transport_pipe::Stream`] with blocking reads, as the TCP pump of `coqui-link` does for a socket.
//!
//! A Unix socket half-closes (dropping the write half is `shutdown(Write)`); a Windows pipe does not, so there a `finish`
//! from the stream ends both directions and a reply the pipe's server would send after the client's end is lost.

use std::io::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use coqui_link::forward::Options;
use coqui_link::mux::{CloseReason, LinkStream, StreamLife};
use coqui_transport::{Channel, Failure, Split};
use coqui_transport_pipe::{Error, Listener, PipeName, Stream};

const FAILED: &str = "the connection to the target failed";

/// Connects to the socket or pipe `name` within `timeout`. Reads wake every [`Options::watch`], so each direction notices
/// the end of the stream, and a write that takes [`Options::stalled`] fails (on Windows a pipe cannot be written to again
/// after that).
///
/// # Errors
///
/// What [`Stream::connect`] reports.
pub fn connect(name: &PipeName, timeout: Duration, options: &Options) -> Result<Stream, Error> {
    Stream::builder()
        .name(name)
        .timeout(timeout)
        .read_timeout(Some(options.watch))
        .write_timeout(Some(options.stalled))
        .build()
        .map_err(|error| match error {
            coqui_transport_pipe::BuildError::Failed(error) => error,
            other => Error::Io(io::Error::other(other.to_string())),
        })
}

/// Binds the socket or pipe `name` for a local forward, with the crate's defaults (this user alone, mode `0o600`) and the
/// reads and writes of [`connect`].
///
/// # Errors
///
/// What [`Listener::builder`] reports.
pub fn listener(name: &PipeName, options: &Options) -> Result<Listener, Error> {
    Listener::builder()
        .name(name)
        .read_timeout(Some(options.watch))
        .write_timeout(Some(options.stalled))
        .build()
        .map_err(|error| match error {
            coqui_transport_pipe::BuildError::Failed(error) => error,
            other => Error::Io(io::Error::other(other.to_string())),
        })
}

/// Moves bytes between `stream` and `ipc` until both directions have ended, as [`coqui_link::forward::pump_with`] does for
/// TCP: the stream's `finish` ends what is written to `ipc` and `ipc`'s end finishes the stream; a stream that ends any other
/// way ends the connection at once.
pub fn pump<S: LinkStream + ?Sized>(stream: &S, ipc: Stream, options: &Options) {
    if options.validate().is_err() {
        stream.close_with(CloseReason::Refused, FAILED);
        return;
    }
    let Ok(halves) = ipc.split() else {
        stream.close_with(CloseReason::Refused, FAILED);
        return;
    };
    let coqui_transport::Halves {
        reader,
        mut writer,
        interrupt,
        ..
    } = halves;
    let stop = AtomicBool::new(false);
    let end = || {
        stop.store(true, Ordering::SeqCst);
        if let Some(interrupt) = &interrupt {
            interrupt.interrupt();
        }
    };
    thread::scope(|scope| {
        scope.spawn(|| ipc_to_stream(stream, reader, options, &stop, &end));
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match stream.receive(Some(options.watch)) {
                Ok(Some(message)) => {
                    if writer.write_all(&message).is_err() {
                        Channel::close(stream);
                        end();
                        break;
                    }
                }
                // Dropping the write half is the end of what the target reads.
                Ok(None) => break,
                Err(Failure::Timeout) => {}
                Err(_) => {
                    end();
                    break;
                }
            }
        }
        drop(writer);
    });
    end();
}

fn ipc_to_stream<S: LinkStream + ?Sized>(
    stream: &S,
    mut reader: impl io::Read,
    options: &Options,
    stop: &AtomicBool,
    end: &dyn Fn(),
) {
    let mut buffer = vec![0_u8; options.chunk];
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match stream.life() {
            StreamLife::Open => {}
            StreamLife::Draining => return,
            StreamLife::Gone => return end(),
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = stream.finish();
                return;
            }
            Ok(read) => {
                if !send_all(stream, &buffer[..read], options, stop) {
                    return end();
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => {
                stream.close_with(CloseReason::Refused, FAILED);
                return end();
            }
        }
    }
}

/// `false` when it could not all go. A full window is only a wait, for as long as the stream is alive.
pub(super) fn send_all<S: LinkStream + ?Sized>(
    stream: &S,
    bytes: &[u8],
    options: &Options,
    stop: &AtomicBool,
) -> bool {
    loop {
        match stream.send(bytes) {
            Ok(()) => return true,
            Err(Failure::NoProgress | Failure::Timeout)
                if !stop.load(Ordering::SeqCst) && stream.life() == StreamLife::Open =>
            {
                thread::sleep(options.idle_min);
            }
            Err(_) => return false,
        }
    }
}
