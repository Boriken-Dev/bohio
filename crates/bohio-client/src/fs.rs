//! Files on a host: the filesystem calls and the two transfer streams.

use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bohio_proto::compress::{Codec, Compression};
use bohio_transfer::compress::{Bounds, Caps, DecodeError, Decoder, Encoder, Piece};

use bohio_proto::control::{Call, Reply};
use bohio_proto::fs::{
    DeltaCopy, Entry, Listing, Metadata, ReadHeader, ReadOutput, ReadRequest, Ready, Timestamp,
    WriteDone, WriteInput, WriteOutput, WriteRequest,
};
use bohio_proto::hash::Digests;
use bohio_proto::protocol;
use coqui_transport::Channel;

use crate::control::Control;
use crate::{Error, Open, OpenOptions};

/// What a [`Fs::copy_with`] does besides copying the bytes of a file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyOptions {
    /// Replace an existing target; a directory is merged into an existing one.
    pub overwrite: bool,
    /// Copy a directory with what it holds.
    pub recursive: bool,
    /// Give the copy the original's permission bits (without the set-ID bits) and modification time.
    pub preserve: bool,
}

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

    /// Metadata of every path in `paths`, in order, each its own result: the calls are all on the way before the first
    /// answer is read, so the batch costs about one round trip. `follow` as for [`Fs::stat`].
    pub fn stat_many(&self, paths: &[&str], follow: bool) -> Vec<Result<Metadata, Error>> {
        let calls = paths
            .iter()
            .map(|path| Call::Stat {
                path: (*path).to_owned(),
                follow,
            })
            .collect();
        self.control
            .call_many(calls, self.timeout)
            .into_iter()
            .map(|answer| match answer? {
                Reply::Stat(meta) => Ok(*meta),
                _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
            })
            .collect()
    }

    /// Every entry of each directory in `paths`, in order, each its own result. The first pages of all of them are asked
    /// at once, then the next pages of those that have more, so the batch costs one round trip per page of the longest
    /// directory rather than one per page of every one. (One directory's pages cannot be asked ahead: each page's cursor
    /// is in the one before.)
    pub fn list_many(&self, paths: &[&str]) -> Vec<Result<Vec<Entry>, Error>> {
        let mut found: Vec<Result<Vec<Entry>, Error>> =
            paths.iter().map(|_| Ok(Vec::new())).collect();
        let mut more: Vec<(usize, Option<String>)> =
            (0..paths.len()).map(|at| (at, None)).collect();
        while !more.is_empty() {
            let calls = more
                .iter()
                .map(|(at, after)| Call::List {
                    path: paths[*at].to_owned(),
                    after: after.clone(),
                    limit: None,
                })
                .collect();
            let answers = self.control.call_many(calls, self.timeout);
            let mut next = Vec::new();
            for ((at, _), answer) in more.into_iter().zip(answers) {
                match answer {
                    Ok(Reply::List(page)) => {
                        if let Ok(all) = &mut found[at] {
                            all.extend(page.entries);
                        }
                        if let Some(after) = page.next {
                            next.push((at, Some(after)));
                        }
                    }
                    Ok(_) => found[at] = Err(Error::Protocol(bohio_proto::Error::Decode)),
                    Err(error) => found[at] = Err(error),
                }
            }
            more = next;
        }
        found
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
        self.copy_with(
            from,
            to,
            &CopyOptions {
                overwrite,
                ..CopyOptions::default()
            },
        )
    }

    /// Copies `from` to `to` on the host, never through this client: a regular file, or with
    /// `options.recursive` a directory and everything in it (links stay links).
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`]; `exists` when `to` exists and not `options.overwrite`, `busy` past the host's
    /// `[filesystem] max_tree_entries`.
    pub fn copy_with(&self, from: &str, to: &str, options: &CopyOptions) -> Result<(), Error> {
        self.done(Call::Copy {
            from: from.to_owned(),
            to: to.to_owned(),
            overwrite: options.overwrite,
            recursive: options.recursive,
            preserve: options.preserve,
        })
    }

    /// Moves `from` to `to` on the host: a rename where the host can, else (another filesystem) a copy that
    /// keeps permission bits and modification time and then the removal of the original. `exists` when `to`
    /// exists and not `overwrite`.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`]; `busy` past the host's `[filesystem] max_tree_entries` for a tree.
    pub fn move_to(&self, from: &str, to: &str, overwrite: bool) -> Result<(), Error> {
        self.done(Call::Move {
            from: from.to_owned(),
            to: to.to_owned(),
            overwrite,
        })
    }

    /// Sets the regular file `path` to `length` bytes: a shorter length cuts it, a longer one extends it with
    /// zeros.
    ///
    /// # Errors
    ///
    /// As [`Fs::stat`].
    pub fn truncate(&self, path: &str, length: u64) -> Result<(), Error> {
        self.done(Call::Truncate {
            path: path.to_owned(),
            length,
        })
    }

    /// Opens a second control stream with the same timeout: the host answers each stream's calls in order, so a
    /// long `copy` or `move` of a tree holds its stream and the calls that follow it wait. Run it on this one
    /// and keep the first for the short calls.
    ///
    /// # Errors
    ///
    /// Whatever `link` reports.
    pub fn spawn_control(&self, link: &dyn Open) -> Result<Self, Error> {
        Ok(Self::connect(link)?.with_timeout(self.timeout))
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A decode failure as the client's error: the data is not what the header promised, or this build
/// cannot decode the algorithm.
fn decode_error(error: &DecodeError) -> Error {
    match error {
        DecodeError::Unsupported(_) | DecodeError::Setup(_) => {
            Error::Compression(error.to_string())
        }
        _ => Error::Protocol(bohio_proto::Error::Decode),
    }
}

/// What a read does beyond the request.
#[derive(Default)]
pub struct ReadOptions {
    /// The largest zstd window the reader accepts, as a power of two; a stream asking for more is refused
    /// before it costs memory. Default `27` (128 MiB), zstd's own.
    pub window_log_max: Option<u32>,
    /// Hand out the bytes as they travel instead of decoding them: the compressed stream of a prepared
    /// read, to save and decode later. A read that resumes is always this way, since a decoder cannot
    /// start in the middle of a stream.
    pub raw: bool,
    /// Told how many bytes of the file the host has compressed so far while it prepares a read. Each call
    /// also restarts the wait for the header.
    pub on_progress: Option<Box<dyn FnMut(u64) + Send>>,
}

impl core::fmt::Debug for ReadOptions {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadOptions")
            .field("window_log_max", &self.window_log_max)
            .field("raw", &self.raw)
            .finish_non_exhaustive()
    }
}

/// A file being read from a host: `bohio-read/1`. Dropping it closes the stream, which stops the
/// host sending.
pub struct Reader {
    channel: Box<dyn Channel>,
    header: ReadHeader,
    received: AtomicU64,
    /// The bytes of data and compressed frames that arrived, before decoding.
    wire: AtomicU64,
    digests: Mutex<Option<Digests>>,
    /// The decoder of the compressed frames; `None` for a raw read or one handed out undecoded.
    decoder: Mutex<Option<Decoder>>,
    /// What `Done` must find received: the file's length, or for an undecoded prepared stream the
    /// compressed bytes still to come.
    expected: Option<u64>,
    /// Whether the frames are decoded here (so the header's length bounds them).
    decoded: bool,
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
        Self::start_with(link, request, timeout, ReadOptions::default())
    }

    /// As [`Reader::start`], with `options`. A prepared read can take as long as the host needs to compress
    /// the range: its `progress` frames restart the wait, so `timeout` is how long the host may be silent.
    ///
    /// # Errors
    ///
    /// As [`Reader::start`]; [`Error::Compression`] when the host answers with an algorithm this build
    /// cannot decode.
    pub fn start_with(
        link: &dyn Open,
        request: &ReadRequest,
        timeout: Option<Duration>,
        mut options: ReadOptions,
    ) -> Result<Self, Error> {
        let open = OpenOptions {
            receive_window: link.bulk_window(),
        };
        let channel = link.open_with(protocol::READ, &request.to_metadata()?, &open)?;
        let mut deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = channel.receive(left)? else {
                return Err(Error::Closed);
            };
            match ReadOutput::decode(&frame) {
                Ok(ReadOutput::Header(header)) => {
                    let raw = options.raw || request.resume.is_some();
                    let compressed = header
                        .compression
                        .as_ref()
                        .filter(|used| used.algorithm.compresses());
                    let decoder = match (compressed, raw) {
                        (Some(used), false) => Some(
                            Decoder::new(
                                used,
                                &Bounds {
                                    window_log_max: options.window_log_max.unwrap_or(27),
                                    max_output: Some(header.length),
                                    max_expansion: 0,
                                },
                            )
                            .map_err(|error| decode_error(&error))?,
                        ),
                        _ => None,
                    };
                    let expected = if raw && compressed.is_some() {
                        header.compressed_length.map(|all| {
                            all.saturating_sub(request.resume.as_ref().map_or(0, |at| at.offset))
                        })
                    } else if raw {
                        None
                    } else {
                        Some(header.length)
                    };
                    return Ok(Self {
                        channel,
                        header,
                        received: AtomicU64::new(0),
                        wire: AtomicU64::new(0),
                        digests: Mutex::new(None),
                        decoder: Mutex::new(decoder),
                        expected,
                        decoded: !raw,
                    });
                }
                Ok(ReadOutput::Progress(progress)) => {
                    if let Some(tell) = options.on_progress.as_mut() {
                        tell(progress.prepared);
                    }
                    deadline = timeout.map(|limit| Instant::now() + limit);
                }
                Ok(ReadOutput::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// The file's size and how many bytes this read will deliver once decoded, and what the host used.
    pub fn header(&self) -> ReadHeader {
        self.header.clone()
    }

    /// How many bytes of data frames arrived so far, as they travelled (compressed where they were): with the
    /// bytes delivered by [`Reader::next_chunk`] it is the compression ratio of this read.
    pub fn wire_bytes(&self) -> u64 {
        self.wire.load(Ordering::Relaxed)
    }

    /// What the host compresses this read with (the level it clamped to): `None` for a raw read.
    pub const fn compression(&self) -> Option<&Compression> {
        self.header.compression.as_ref()
    }

    /// The digests of the bytes the host sent, those the request's `hashes` named, once the last chunk
    /// has been read; `None` before that.
    pub fn digests(&self) -> Option<Digests> {
        self.digests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The next chunk of the file, decoded, waiting up to `timeout`; `Ok(None)` once all of it arrived.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host failed the read, [`Error::Timeout`] (the stream goes on),
    /// [`Error::Closed`] when the stream ended before the last frame or delivered a different
    /// number of bytes than it announced, [`Error::Protocol`] when the data is not valid or is longer
    /// than the header said.
    pub fn next_chunk(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = self.channel.receive(left)? else {
                return Err(Error::Closed);
            };
            match ReadOutput::decode(&frame) {
                Ok(ReadOutput::Data(bytes)) => {
                    self.wire.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    // A raw frame ends the compression context.
                    if let Some(decoder) = lock(&self.decoder).as_mut() {
                        decoder.reset();
                    }
                    self.count(bytes.len() as u64)?;
                    return Ok(Some(bytes));
                }
                Ok(ReadOutput::Compressed(piece)) => {
                    self.wire.fetch_add(piece.len() as u64, Ordering::Relaxed);
                    let mut decoded = Vec::new();
                    let mut guard = lock(&self.decoder);
                    let Some(decoder) = guard.as_mut() else {
                        drop(guard);
                        // Handed out as it travels.
                        self.count(piece.len() as u64)?;
                        return Ok(Some(piece));
                    };
                    decoder
                        .feed(&piece, &mut decoded)
                        .map_err(|error| decode_error(&error))?;
                    drop(guard);
                    if decoded.is_empty() {
                        continue;
                    }
                    self.count(decoded.len() as u64)?;
                    return Ok(Some(decoded));
                }
                Ok(ReadOutput::Done(done)) => {
                    return if self
                        .expected
                        .is_none_or(|expected| self.received.load(Ordering::Relaxed) == expected)
                    {
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

    fn count(&self, bytes: u64) -> Result<(), Error> {
        let total = self.received.fetch_add(bytes, Ordering::Relaxed) + bytes;
        // A host that sends more than it announced is not trusted with more memory or disk.
        if self.decoded && total > self.header.length {
            return Err(Error::Protocol(bohio_proto::Error::Decode));
        }
        Ok(())
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
    /// The encoder of what the request declared it sends compressed.
    encoder: Mutex<Option<Encoder>>,
    /// The bytes of the frames sent so far, as they travelled.
    sent: AtomicU64,
    /// What the host's `ready` said: the token of a resumable write, or where a resumed one goes on.
    ready: Ready,
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
        Self::start_with(
            link,
            request,
            timeout,
            &Caps {
                window_log_max: 27,
                ..Caps::default()
            },
        )
    }

    /// As [`Writer::start`], compressing within `caps` (the levels and window this side may use: the
    /// defaults stop at zstd level 19 and a window of 8 MiB, which the host's own caps may lower further).
    /// The request's `compression` names the algorithm, never `auto`: a client reads the host's default
    /// from `info.transfer.compression_default` ([`resolve_auto`]).
    ///
    /// # Errors
    ///
    /// As [`Writer::start`]; [`Error::Compression`] when the algorithm is `auto` or this build lacks it.
    pub fn start_with(
        link: &dyn Open,
        request: &WriteRequest,
        timeout: Option<Duration>,
        caps: &Caps,
    ) -> Result<Self, Error> {
        let encoder = match &request.compression {
            Some(wanted) if wanted.algorithm.compresses() => Some(
                Encoder::new(wanted, caps)
                    .map_err(|error| Error::Compression(error.to_string()))?,
            ),
            Some(wanted) if wanted.algorithm == Codec::Auto => {
                return Err(Error::Compression(
                    "name the algorithm to send with: auto is for what the host sends".to_owned(),
                ));
            }
            _ => None,
        };
        let channel = link.open(protocol::WRITE, &request.to_metadata()?)?;
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = channel.receive(left)? else {
                return Err(Error::Closed);
            };
            match WriteOutput::decode(&frame) {
                Ok(WriteOutput::Ready(ready)) => {
                    return Ok(Self {
                        channel,
                        chunk: Self::DEFAULT_CHUNK,
                        encoder: Mutex::new(encoder),
                        sent: AtomicU64::new(0),
                        ready,
                    });
                }
                Ok(WriteOutput::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// For a write that asked to `resume`: the token that continues it if the stream ends early.
    pub fn token(&self) -> Option<&str> {
        self.ready.token.as_deref()
    }

    /// For a write in mode `resume`: how many bytes of the file the host holds. The data sent from here on
    /// is the file from this offset.
    pub const fn resumed_at(&self) -> Option<u64> {
        self.ready.offset
    }

    /// Asks the host to make what was written so far safe (`append` and `at` writes only) and waits up to
    /// `timeout` for it to say how many bytes this stream has written. Any other write mode is `bad_request`
    /// and ends the stream.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host refused, [`Error::Timeout`], [`Error::Closed`].
    pub fn flush_remote(&self, timeout: Option<Duration>) -> Result<u64, Error> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        self.send(&WriteInput::Flush.encode())?;
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(frame) = self.channel.receive(left)? else {
                return Err(Error::Closed);
            };
            match WriteOutput::decode(&frame) {
                Ok(WriteOutput::Flushed(flushed)) => return Ok(flushed.written),
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
                compressed.map_or_else(|| WriteInput::Data(piece.to_vec()), WriteInput::Compressed);
            self.send(&frame.encode())?;
        }
        Ok(())
    }

    /// For a delta write: the host takes `count` blocks of the target from block `block` as they are. A copy
    /// neither ends nor disturbs the compression context.
    ///
    /// # Errors
    ///
    /// As [`Writer::write`].
    pub fn copy(&self, block: u64, count: u64) -> Result<(), Error> {
        self.send(&WriteInput::Copy(DeltaCopy { block, count }).encode())
    }

    /// How many bytes of frames this writer sent so far, as they travelled (compressed where they were): with
    /// the bytes the file holds it is the compression ratio, and for a delta the cost of the write.
    pub fn wire_bytes(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Sends a piece of a stream compressed elsewhere (a prepared transfer), as it is: the request must
    /// have declared its compression. Pieces may be cut anywhere in the stream.
    ///
    /// # Errors
    ///
    /// As [`Writer::write`].
    pub fn write_compressed(&self, bytes: &[u8]) -> Result<(), Error> {
        let room = self
            .channel
            .capabilities()
            .max_send
            .map_or(self.chunk, |max| self.chunk.min(max.saturating_sub(1)))
            .max(1);
        for piece in bytes.chunks(room) {
            self.send(&WriteInput::Compressed(piece.to_vec()).encode())?;
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
            Ok(()) => {
                self.sent.fetch_add(message.len() as u64, Ordering::Relaxed);
                Ok(())
            }
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

/// What `auto` means on a host that reported `info`: its default compression, the one a client that must
/// name an algorithm (for what it sends) uses. `None` when the host reports none.
pub fn resolve_auto(info: &bohio_proto::info::Info, wanted: &Compression) -> Option<Compression> {
    if wanted.algorithm != Codec::Auto {
        return Some(wanted.clone());
    }
    let default = info.transfer.compression_default.clone()?;
    Some(Compression {
        algorithm: default.algorithm,
        level: wanted.level.or(default.level),
        window_log: wanted.window_log.or(default.window_log),
    })
}

/// How [`put`] sends a file.
#[derive(Debug, Clone, Default)]
pub struct PutOptions {
    /// How to compress what is sent; never `auto` (see [`resolve_auto`]). `None` sends it raw.
    pub compression: Option<Compression>,
    /// Compress everything into a temporary file first, then send it: one compression context over the
    /// whole stream (the best ratio, which a slow link pays for once) at the price of the time and disk it
    /// takes before the first byte leaves.
    pub prepare: bool,
    /// The levels and window this side may use.
    pub caps: Caps,
    /// Where the temporary file of a prepared put is made; the system's when absent.
    pub temp_dir: Option<std::path::PathBuf>,
    /// The most bytes in one frame; [`Writer::DEFAULT_CHUNK`] when absent.
    pub chunk: Option<usize>,
}

/// Sends everything `source` yields to the host as `request`'s file and waits for it to be put in place.
/// `request.compression` is replaced by `options.compression`.
///
/// # Errors
///
/// As [`Writer::start`], [`Writer::write`] and [`Writer::finish`]; a failing `source` is
/// [`Error::Closed`].
pub fn put(
    link: &dyn Open,
    request: &WriteRequest,
    source: &mut dyn Read,
    options: &PutOptions,
    timeout: Option<Duration>,
) -> Result<WriteDone, Error> {
    let mut request = request.clone();
    request.compression.clone_from(&options.compression);
    let chunk = options.chunk.unwrap_or(Writer::DEFAULT_CHUNK).max(1);
    let spool = match (&options.compression, options.prepare) {
        (Some(wanted), true) if wanted.algorithm.compresses() => {
            Some(spool(source, wanted, options, chunk)?)
        }
        _ => None,
    };
    let writer = Writer::start_with(link, &request, timeout, &options.caps)?.with_chunk(chunk);
    if let Some(mut file) = spool {
        let mut buffer = vec![0_u8; chunk];
        loop {
            let count = file.read(&mut buffer).map_err(|_| Error::Closed)?;
            if count == 0 {
                break;
            }
            writer.write_compressed(&buffer[..count])?;
        }
    } else {
        let mut buffer = vec![0_u8; chunk];
        loop {
            let count = match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Error::Closed),
            };
            writer.write(&buffer[..count])?;
        }
    }
    writer.finish(timeout)
}

/// Compresses `source` into one stream in an anonymous temporary file, rewound to its start.
fn spool(
    source: &mut dyn Read,
    wanted: &Compression,
    options: &PutOptions,
    chunk: usize,
) -> Result<std::fs::File, Error> {
    let mut encoder = Encoder::new(wanted, &options.caps)
        .map_err(|error| Error::Compression(error.to_string()))?;
    let mut file = match &options.temp_dir {
        Some(dir) => tempfile::tempfile_in(dir),
        None => tempfile::tempfile(),
    }
    .map_err(|_| Error::Closed)?;
    let mut buffer = vec![0_u8; chunk];
    loop {
        let count = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Error::Closed),
        };
        let piece = encoder
            .piece(&buffer[..count], false)
            .map_err(|error| Error::Compression(error.to_string()))?;
        file.write_all(&piece).map_err(|_| Error::Closed)?;
    }
    let tail = encoder
        .finish()
        .map_err(|error| Error::Compression(error.to_string()))?;
    file.write_all(&tail).map_err(|_| Error::Closed)?;
    file.seek(SeekFrom::Start(0)).map_err(|_| Error::Closed)?;
    Ok(file)
}
