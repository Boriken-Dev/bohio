//! Files on a host: the filesystem calls and the two transfer streams.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bohio_proto::control::{Call, Reply};
use bohio_proto::fs::{
    Entry, Listing, Metadata, ReadHeader, ReadOutput, ReadRequest, Timestamp, WriteDone,
    WriteInput, WriteOutput, WriteRequest,
};
use bohio_proto::hash::Digests;
use bohio_proto::protocol;
use coqui_transport::Channel;

use crate::control::Control;
use crate::{Error, Open, OpenOptions};

/// The filesystem calls of one host, over a control stream.
#[derive(Debug)]
pub struct Fs {
    control: Control,
    timeout: Option<Duration>,
}

impl Fs {
    /// Opens a control stream on `link`. Calls wait without limit until [`Fs::with_timeout`].
    ///
    /// # Errors
    ///
    /// Whatever `link` reports.
    pub fn connect(link: &dyn Open) -> Result<Self, Error> {
        Ok(Self {
            control: Control::open(link)?,
            timeout: None,
        })
    }

    /// Bounds how long each call waits for its answer.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// The control stream the calls use.
    pub const fn control(&self) -> &Control {
        &self.control
    }

    fn done(&self, call: Call) -> Result<(), Error> {
        match self.control.call(call, self.timeout)? {
            Reply::Done => Ok(()),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// Metadata of `path`; a final symbolic link is described itself unless `follow`.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] with `not_found`, `outside_root`, `denied`, ...; transport errors.
    pub fn stat(&self, path: &str, follow: bool) -> Result<Metadata, Error> {
        match self.control.call(
            Call::Stat {
                path: path.to_owned(),
                follow,
            },
            self.timeout,
        )? {
            Reply::Stat(meta) => Ok(*meta),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// One page of the directory `path`, after the name `after`.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`].
    pub fn list(
        &self,
        path: &str,
        after: Option<&str>,
        limit: Option<u64>,
    ) -> Result<Listing, Error> {
        match self.control.call(
            Call::List {
                path: path.to_owned(),
                after: after.map(str::to_owned),
                limit,
            },
            self.timeout,
        )? {
            Reply::List(listing) => Ok(listing),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// Every entry of the directory `path`, page after page.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`].
    pub fn list_all(&self, path: &str) -> Result<Vec<Entry>, Error> {
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = self.list(path, after.as_deref(), None)?;
            all.extend(page.entries);
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(all),
            }
        }
    }

    /// Creates the directory `path`, and its missing parents when `parents`.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`]; `exists` for an existing directory without `parents`.
    pub fn mkdir(&self, path: &str, parents: bool) -> Result<(), Error> {
        self.done(Call::Mkdir {
            path: path.to_owned(),
            parents,
            mode: None,
        })
    }

    /// Removes a file or link, or a directory (with everything in it when `recursive`).
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`].
    pub fn remove(&self, path: &str, recursive: bool) -> Result<(), Error> {
        self.done(Call::Remove {
            path: path.to_owned(),
            recursive,
        })
    }

    /// Moves `from` to `to`; `exists` when `to` exists and not `overwrite`.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`].
    pub fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<(), Error> {
        self.done(Call::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
            overwrite,
        })
    }

    /// Copies the regular file `from` to `to` on the host.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`].
    pub fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<(), Error> {
        self.done(Call::Copy {
            from: from.to_owned(),
            to: to.to_owned(),
            overwrite,
        })
    }

    /// Creates the symbolic link `link` holding the relative text `target`.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`]; `outside_root` for a target that is absolute or climbs out of the roots.
    pub fn symlink(&self, target: &str, link: &str) -> Result<(), Error> {
        self.done(Call::Symlink {
            target: target.to_owned(),
            link: link.to_owned(),
        })
    }

    /// The text of the symbolic link `path`.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`].
    pub fn readlink(&self, path: &str) -> Result<String, Error> {
        match self.control.call(
            Call::Readlink {
                path: path.to_owned(),
            },
            self.timeout,
        )? {
            Reply::Link { target } => Ok(target),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        }
    }

    /// Sets the access and modification times that are given.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`].
    pub fn set_times(
        &self,
        path: &str,
        accessed: Option<Timestamp>,
        modified: Option<Timestamp>,
    ) -> Result<(), Error> {
        self.done(Call::SetTimes {
            path: path.to_owned(),
            accessed,
            modified,
        })
    }

    /// Sets Unix permission bits; `unsupported` on a host without them.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`].
    pub fn set_mode(&self, path: &str, mode: u32) -> Result<(), Error> {
        self.done(Call::SetMode {
            path: path.to_owned(),
            mode,
        })
    }

    /// Sets the Windows read-only attribute; `unsupported` on other hosts.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`].
    pub fn set_readonly(&self, path: &str, readonly: bool) -> Result<(), Error> {
        self.done(Call::SetAttributes {
            path: path.to_owned(),
            readonly: Some(readonly),
        })
    }
}

/// A file being read from a host: `bohio-read/1`. Dropping it closes the stream, which stops the
/// host sending.
pub struct Reader {
    channel: Box<dyn Channel>,
    header: ReadHeader,
    received: AtomicU64,
    digests: Mutex<Option<Digests>>,
}

impl core::fmt::Debug for Reader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Reader")
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

impl Reader {
    /// Opens a read stream for `request` and waits up to `timeout` for the host to open the file.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host refused (`denied`, `not_found`, `outside_root`, ...),
    /// [`Error::Timeout`], [`Error::Closed`], or whatever `link` reports.
    pub fn start(
        link: &dyn Open,
        request: &ReadRequest,
        timeout: Option<Duration>,
    ) -> Result<Self, Error> {
        let options = OpenOptions {
            receive_window: link.bulk_window(),
        };
        let channel = link.open_with(protocol::READ, &request.to_metadata()?, &options)?;
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = channel.receive(left)? else {
                return Err(Error::Closed);
            };
            match ReadOutput::decode(&frame) {
                Ok(ReadOutput::Header(header)) => {
                    return Ok(Self {
                        channel,
                        header,
                        received: AtomicU64::new(0),
                        digests: Mutex::new(None),
                    });
                }
                Ok(ReadOutput::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// The file's size and how many bytes this read will deliver.
    pub const fn header(&self) -> ReadHeader {
        self.header
    }

    /// The digests of the bytes the host sent, those the request's `hashes` named, once the last chunk
    /// has been read; `None` before that.
    pub fn digests(&self) -> Option<Digests> {
        self.digests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The next chunk of the file, waiting up to `timeout`; `Ok(None)` once all of it arrived.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host failed the read, [`Error::Timeout`] (the stream goes on),
    /// [`Error::Closed`] when the stream ended before the last frame or delivered a different
    /// number of bytes than it announced.
    pub fn next_chunk(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = self.channel.receive(left)? else {
                return Err(Error::Closed);
            };
            match ReadOutput::decode(&frame) {
                Ok(ReadOutput::Data(bytes)) => {
                    self.received
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    return Ok(Some(bytes));
                }
                Ok(ReadOutput::Done(done)) => {
                    return if self.received.load(Ordering::Relaxed) == self.header.length {
                        *self
                            .digests
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(done.hashes);
                        Ok(None)
                    } else {
                        Err(Error::Closed)
                    };
                }
                Ok(ReadOutput::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Writes the rest of the file to `out` chunk by chunk, so memory stays at one chunk; returns
    /// the bytes written. `timeout` bounds the wait for each chunk.
    ///
    /// # Errors
    ///
    /// As [`Reader::next_chunk`]; a failing `out` is [`Error::Closed`].
    pub fn copy_to(&self, out: &mut impl Write, timeout: Option<Duration>) -> Result<u64, Error> {
        let mut total = 0_u64;
        while let Some(chunk) = self.next_chunk(timeout)? {
            out.write_all(&chunk).map_err(|_| Error::Closed)?;
            total += chunk.len() as u64;
        }
        Ok(total)
    }

    /// The rest of the file in memory.
    ///
    /// # Errors
    ///
    /// As [`Reader::next_chunk`].
    pub fn read_to_end(&self, timeout: Option<Duration>) -> Result<Vec<u8>, Error> {
        let mut all = Vec::new();
        self.copy_to(&mut all, timeout)?;
        Ok(all)
    }

    /// Closes the stream; the host stops reading.
    pub fn close(&self) {
        self.channel.close();
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.channel.close();
    }
}

/// A file being written on a host: `bohio-write/1`. Dropping it before [`Writer::finish`] closes
/// the stream, which makes the host abandon the write.
pub struct Writer {
    channel: Box<dyn Channel>,
    chunk: usize,
}

impl core::fmt::Debug for Writer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Writer").finish_non_exhaustive()
    }
}

impl Writer {
    /// The most file bytes in one frame until [`Writer::with_chunk`] says otherwise.
    pub const DEFAULT_CHUNK: usize = 64 * 1024;

    /// Opens a write stream for `request` and waits up to `timeout` for the host to open the
    /// target.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host refused (`denied`, `outside_root`, `exists`, ...),
    /// [`Error::Timeout`], [`Error::Closed`], or whatever `link` reports.
    pub fn start(
        link: &dyn Open,
        request: &WriteRequest,
        timeout: Option<Duration>,
    ) -> Result<Self, Error> {
        let channel = link.open(protocol::WRITE, &request.to_metadata()?)?;
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = channel.receive(left)? else {
                return Err(Error::Closed);
            };
            match WriteOutput::decode(&frame) {
                Ok(WriteOutput::Ready) => {
                    return Ok(Self {
                        channel,
                        chunk: Self::DEFAULT_CHUNK,
                    });
                }
                Ok(WriteOutput::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Sets the most file bytes in one frame (at least 1); a smaller message bound of the channel
    /// wins. A stream whose sender queue is smaller than a frame never takes it, so keep this
    /// below what the link queues.
    #[must_use]
    pub fn with_chunk(mut self, bytes: usize) -> Self {
        self.chunk = bytes.max(1);
        self
    }

    /// Sends file bytes in frames of at most the chunk size, waiting until the channel takes them.
    ///
    /// # Errors
    ///
    /// The host's error if it already failed the write, else [`Error::Transport`].
    pub fn write(&self, bytes: &[u8]) -> Result<(), Error> {
        let room = self
            .channel
            .capabilities()
            .max_send
            .map_or(self.chunk, |max| self.chunk.min(max.saturating_sub(1)))
            .max(1);
        for piece in bytes.chunks(room) {
            self.send(&WriteInput::Data(piece.to_vec()).encode())?;
        }
        Ok(())
    }

    /// Ends the data and waits up to `timeout` for the host to put it in place.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host failed the write (nothing was put in place for `replace` and
    /// `create`), [`Error::Timeout`], [`Error::Closed`].
    pub fn finish(&self, timeout: Option<Duration>) -> Result<WriteDone, Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        self.send(&WriteInput::End.encode())?;
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = self.channel.receive(left)? else {
                return Err(Error::Closed);
            };
            match WriteOutput::decode(&frame) {
                Ok(WriteOutput::Done(done)) => return Ok(done),
                Ok(WriteOutput::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Closes the stream without ending the data: the host abandons the write.
    pub fn abort(&self) {
        self.channel.close();
    }

    fn send(&self, message: &[u8]) -> Result<(), Error> {
        match self.channel.send_timeout(message, None) {
            Ok(()) => Ok(()),
            Err(failure) => {
                // The host may have sent why it gave up before the stream failed.
                while let Ok(Some(frame)) = self.channel.receive(Some(Duration::ZERO)) {
                    if let Ok(WriteOutput::Error(body)) = WriteOutput::decode(&frame) {
                        return Err(Error::Remote(body));
                    }
                }
                Err(failure.into())
            }
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.channel.close();
    }
}
