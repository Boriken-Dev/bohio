//! Many files on one stream: a bundle of `bohio-read/1` or `bohio-write/1`.
//!
//! A tree of small files costs a round trip, an open and a fresh compression context for each file when each has a
//! stream of its own. A bundle opens one stream, sends the file names (or the files), and keeps one compression
//! context across them all; every file still succeeds or fails alone.

use std::io::Read;
use std::thread;
use std::time::Duration;

use bohio_proto::compress::{Codec, Compression};
use bohio_proto::fs::{
    ReadHeader, ReadInput, ReadItem, ReadOutput, ReadRequest, WriteDone, WriteInput, WriteItem,
    WriteOutput, WriteRequest,
};
use bohio_proto::hash::{Algorithm, Digests};
use bohio_proto::{ErrorBody, protocol};
use bohio_transfer::compress::{Bounds, Caps, Decoder, Encoder, Piece};
use coqui_transport::Channel;

use crate::{Error, Open, OpenOptions};

/// How a bundle moves its files.
#[derive(Debug, Clone)]
pub struct BundleOptions {
    /// How to compress: a concrete algorithm for [`put_many`] (see `fs::resolve_auto`), anything the host accepts for
    /// [`read_bundle`]. One context spans all the files.
    pub compression: Option<Compression>,
    /// The digests of each file's bytes to answer (of what the host sent for a read, of what it wrote for a write).
    /// For a write, absent means the host's `[filesystem] write_hashes`.
    pub hashes: Option<Vec<Algorithm>>,
    /// The levels and window this side compresses within.
    pub caps: Caps,
    /// The most file bytes in one frame. Default 64 KiB.
    pub chunk: usize,
    /// The largest zstd window a read accepts, as a power of two; 27 when absent.
    pub window_log_max: Option<u32>,
    /// How many times the compressed bytes received the output of a read may grow to, once it passes 1 MiB; `Some(0)`
    /// is no check. Default [`crate::fs::DEFAULT_MAX_EXPANSION`].
    pub max_expansion: Option<u32>,
    /// How long a step may wait for the host; `None` waits without limit. Default five minutes.
    pub timeout: Option<Duration>,
}

impl Default for BundleOptions {
    fn default() -> Self {
        Self {
            compression: None,
            hashes: None,
            caps: Caps {
                window_log_max: 27,
                ..Caps::default()
            },
            chunk: crate::fs::Writer::DEFAULT_CHUNK,
            window_log_max: None,
            max_expansion: None,
            timeout: Some(Duration::from_secs(300)),
        }
    }
}

/// What a bundle read tells its caller, in order.
#[derive(Debug)]
#[non_exhaustive]
pub enum Fetch<'a> {
    /// The next file begins: its header says `index` (its position among the items), size, length, modification time
    /// and permission bits.
    Begin(&'a ReadHeader),
    /// The next bytes of the file that began.
    Data(&'a [u8]),
    /// The file that began ended whole; these are the digests of its bytes that were asked for.
    End {
        /// Which item.
        index: u64,
        /// The digests.
        hashes: Digests,
    },
    /// An item failed. When it follows a `Begin` the bytes received are to be thrown away.
    Failed {
        /// Which item.
        index: u64,
        /// Why.
        error: ErrorBody,
    },
}

/// What a bundle moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BundleStats {
    /// How many items the host answered.
    pub items: u64,
    /// The bytes of frames that arrived (a read) or were sent (a write), as they travelled.
    pub wire_bytes: u64,
}

fn lost() -> Error {
    Error::Closed
}

/// Reads `items` on one stream, calling `on` for what arrives. A failing `on` ends the stream.
///
/// # Errors
///
/// [`Error::Remote`] when the host refused the stream (`denied`, `busy`, `unsupported` for a compression or digest
/// it does not offer) or `on`'s error; [`Error::Closed`] when the stream ended before the host said `end`;
/// [`Error::Protocol`] for a host that sends more than it announced.
pub fn read_bundle(
    link: &dyn Open,
    items: &[ReadItem],
    options: &BundleOptions,
    on: &mut dyn FnMut(Fetch<'_>) -> Result<(), Error>,
) -> Result<BundleStats, Error> {
    let mut request = ReadRequest::bundle();
    request.compression.clone_from(&options.compression);
    request.hashes = options.hashes.clone().unwrap_or_default();
    let open = OpenOptions {
        receive_window: link.bulk_window(),
    };
    let channel = link.open_with(protocol::READ, &request.to_metadata()?, &open)?;
    let channel: &dyn Channel = channel.as_ref();
    thread::scope(|scope| {
        let sender = scope.spawn(move || {
            for item in items {
                let frame = ReadInput::Item(item.clone())
                    .encode()
                    .map_err(Error::from)?;
                channel.send_timeout(&frame, None)?;
            }
            channel.send_timeout(&ReadInput::ItemsEnd.encode().map_err(Error::from)?, None)?;
            Ok::<(), Error>(())
        });
        let received = receive_reads(channel, options, on);
        // Closing ends a send still waiting for room, which a host that gave up no longer makes.
        channel.close();
        let _ = sender.join();
        received
    })
}

fn receive_reads(
    channel: &dyn Channel,
    options: &BundleOptions,
    on: &mut dyn FnMut(Fetch<'_>) -> Result<(), Error>,
) -> Result<BundleStats, Error> {
    let mut stats = BundleStats::default();
    let mut decoder: Option<Decoder> = None;
    let mut begun: Option<(ReadHeader, u64)> = None;
    let mut first = true;
    loop {
        let Some(frame) = channel.receive(options.timeout)? else {
            return Err(lost());
        };
        stats.wire_bytes += frame.len() as u64;
        match ReadOutput::decode(&frame) {
            Ok(ReadOutput::Header(header)) if first => {
                first = false;
                if let Some(used) = header
                    .compression
                    .as_ref()
                    .filter(|used| used.algorithm.compresses())
                {
                    decoder = Some(
                        Decoder::new(
                            used,
                            &Bounds {
                                window_log_max: options.window_log_max.unwrap_or(27),
                                max_output: None,
                                max_expansion: options
                                    .max_expansion
                                    .unwrap_or(crate::fs::DEFAULT_MAX_EXPANSION),
                            },
                        )
                        .map_err(|error| Error::Compression(error.to_string()))?,
                    );
                }
            }
            Ok(ReadOutput::Header(header)) => {
                if header.index.is_none() || begun.is_some() {
                    return Err(Error::Protocol(bohio_proto::Error::Decode));
                }
                on(Fetch::Begin(&header))?;
                begun = Some((header, 0));
            }
            Ok(ReadOutput::Data(bytes)) => {
                // A raw frame ends the compression context.
                if let Some(decoder) = decoder.as_mut() {
                    decoder.reset();
                }
                let (header, got) = begun
                    .as_mut()
                    .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
                *got += bytes.len() as u64;
                if *got > header.length {
                    return Err(Error::Protocol(bohio_proto::Error::Decode));
                }
                on(Fetch::Data(&bytes))?;
            }
            Ok(ReadOutput::Compressed(piece)) => {
                let (header, got) = begun
                    .as_mut()
                    .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
                let decoder = decoder
                    .as_mut()
                    .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
                let mut decoded = Vec::new();
                decoder
                    .feed(&piece, &mut decoded)
                    .map_err(|error| Error::Compression(error.to_string()))?;
                *got += decoded.len() as u64;
                if *got > header.length {
                    return Err(Error::Protocol(bohio_proto::Error::Decode));
                }
                if !decoded.is_empty() {
                    on(Fetch::Data(&decoded))?;
                }
            }
            Ok(ReadOutput::Done(done)) => {
                let (header, got) = begun
                    .take()
                    .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
                if got != header.length {
                    return Err(lost());
                }
                stats.items += 1;
                on(Fetch::End {
                    index: header.index.unwrap_or_default(),
                    hashes: done.hashes,
                })?;
            }
            Ok(ReadOutput::ItemError(failure)) => {
                begun = None;
                stats.items += 1;
                on(Fetch::Failed {
                    index: failure.index,
                    error: failure.body(),
                })?;
            }
            Ok(ReadOutput::End(_)) => return Ok(stats),
            Ok(ReadOutput::Error(body)) => return Err(Error::Remote(body)),
            Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// One file a bundle read brought, in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// What the host said of the file: its size, modification time and permission bits.
    pub header: ReadHeader,
    /// The bytes.
    pub data: Vec<u8>,
    /// The digests asked for.
    pub hashes: Digests,
}

/// Reads `items` on one stream into memory: each file is its own result, so one that is missing does not fail the
/// others. Memory is all the files at once: batch large trees, or use [`read_bundle`] to write each file as it arrives.
///
/// # Errors
///
/// As [`read_bundle`], for the stream as a whole.
pub fn get_many(
    link: &dyn Open,
    items: &[ReadItem],
    options: &BundleOptions,
) -> Result<(Vec<Result<Fetched, ErrorBody>>, BundleStats), Error> {
    let mut results: Vec<Option<Result<Fetched, ErrorBody>>> =
        (0..items.len()).map(|_| None).collect();
    let mut current: Option<Fetched> = None;
    let stats = read_bundle(link, items, options, &mut |event| {
        match event {
            Fetch::Begin(header) => {
                current = Some(Fetched {
                    header: header.clone(),
                    data: Vec::new(),
                    hashes: Digests::new(),
                });
            }
            Fetch::Data(bytes) => {
                if let Some(file) = current.as_mut() {
                    file.data.extend_from_slice(bytes);
                }
            }
            Fetch::End { index, hashes } => {
                if let (Some(mut file), Some(slot)) = (
                    current.take(),
                    usize::try_from(index)
                        .ok()
                        .and_then(|at| results.get_mut(at)),
                ) {
                    file.hashes = hashes;
                    *slot = Some(Ok(file));
                }
            }
            Fetch::Failed { index, error } => {
                current = None;
                if let Some(slot) = usize::try_from(index)
                    .ok()
                    .and_then(|at| results.get_mut(at))
                {
                    *slot = Some(Err(error));
                }
            }
        }
        Ok(())
    })?;
    let all = results
        .into_iter()
        .map(|slot| slot.ok_or(Error::Closed))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((all, stats))
}

/// One file of a bundle write.
pub struct BundleFile<'a> {
    /// Where it goes and how: the path, mode, parents, permission bits, modification time and digests it must have.
    pub item: WriteItem,
    /// Where its bytes come from, read once to the end.
    pub source: &'a mut (dyn Read + Send),
}

impl<'a> BundleFile<'a> {
    /// A file that replaces `path` with what `source` yields.
    pub fn replace(path: impl Into<String>, source: &'a mut (dyn Read + Send)) -> Self {
        Self {
            item: WriteItem::replace(path),
            source,
        }
    }
}

impl core::fmt::Debug for BundleFile<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BundleFile")
            .field("item", &self.item)
            .finish_non_exhaustive()
    }
}

/// What a bundle write brought back.
#[derive(Debug)]
pub struct Put {
    /// Each file's result, in the order given: how many bytes it holds and its digests, or why it was not put in
    /// place (and nothing of it was).
    pub results: Vec<Result<WriteDone, ErrorBody>>,
    /// What travelled.
    pub stats: BundleStats,
}

/// Writes `files` on one stream, each put in place on its own (a temporary sibling renamed, with its modification time set
/// first when the item gives one), so one that fails does not stop the others.
///
/// # Errors
///
/// [`Error::Remote`] when the host refused the stream (`denied`, `busy`, `unsupported`) or ended it with an error;
/// [`Error::Local`] when a source cannot be read (the stream is abandoned: files not yet put in place are not);
/// [`Error::Closed`] when it ended before the host said `end`; [`Error::Compression`].
pub fn put_many(
    link: &dyn Open,
    files: &mut [BundleFile<'_>],
    options: &BundleOptions,
) -> Result<Put, Error> {
    let mut request = WriteRequest::bundle();
    request.compression.clone_from(&options.compression);
    request.hashes.clone_from(&options.hashes);
    let encoder = match &request.compression {
        Some(wanted) if wanted.algorithm.compresses() => Some(
            Encoder::new(wanted, &options.caps)
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
    let channel: &dyn Channel = channel.as_ref();
    // The host's `ready`, or why it will not.
    loop {
        let Some(frame) = channel.receive(options.timeout)? else {
            return Err(Error::Closed);
        };
        match WriteOutput::decode(&frame) {
            Ok(WriteOutput::Ready(_)) => break,
            Ok(WriteOutput::Error(body)) => return Err(Error::Remote(body)),
            Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let count = files.len();
    thread::scope(|scope| {
        let sender = scope.spawn(move || send_files(channel, files, encoder, options));
        let received = receive_writes(channel, count, options);
        channel.close();
        let sent = sender.join().unwrap_or(Err(Error::Closed));
        match (received, sent) {
            (Ok((results, mut stats)), Ok(wire)) => {
                stats.wire_bytes = wire;
                Ok(Put { results, stats })
            }
            // A source that could not be read is the cause, whatever the host then made of the cut stream.
            (_, Err(error @ Error::Local(_))) => Err(error),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    })
}

fn send_files(
    channel: &dyn Channel,
    files: &mut [BundleFile<'_>],
    mut encoder: Option<Encoder>,
    options: &BundleOptions,
) -> Result<u64, Error> {
    let room = channel
        .capabilities()
        .max_send
        .map_or(options.chunk, |max| {
            options.chunk.min(max.saturating_sub(1))
        })
        .max(1);
    let mut wire = 0_u64;
    let mut send = |frame: Vec<u8>| -> Result<(), Error> {
        channel.send_timeout(&frame, None)?;
        wire += frame.len() as u64;
        Ok(())
    };
    let mut buffer = vec![0_u8; room];
    for file in files.iter_mut() {
        send(WriteInput::Item(file.item.clone()).encode())?;
        loop {
            let count = match file.source.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(Error::local("reading a file of the bundle", &error)),
            };
            let piece = &buffer[..count];
            let frame = match encoder.as_mut() {
                Some(encoder) => match encoder
                    .frame(piece)
                    .map_err(|error| Error::Compression(error.to_string()))?
                {
                    Piece::Compressed(compressed) => WriteInput::Compressed(compressed),
                    Piece::Raw => WriteInput::Data(piece.to_vec()),
                },
                None => WriteInput::Data(piece.to_vec()),
            };
            send(frame.encode())?;
        }
        send(WriteInput::End.encode())?;
    }
    send(WriteInput::ItemsEnd.encode())?;
    Ok(wire)
}

fn receive_writes(
    channel: &dyn Channel,
    count: usize,
    options: &BundleOptions,
) -> Result<(Vec<Result<WriteDone, ErrorBody>>, BundleStats), Error> {
    let mut results: Vec<Option<Result<WriteDone, ErrorBody>>> = (0..count).map(|_| None).collect();
    let mut stats = BundleStats::default();
    loop {
        let Some(frame) = channel.receive(options.timeout)? else {
            return Err(lost());
        };
        match WriteOutput::decode(&frame) {
            Ok(WriteOutput::Done(done)) => {
                stats.items += 1;
                let slot = done
                    .index
                    .and_then(|at| usize::try_from(at).ok())
                    .and_then(|at| results.get_mut(at))
                    .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
                *slot = Some(Ok(done));
            }
            Ok(WriteOutput::ItemError(failure)) => {
                stats.items += 1;
                let slot = usize::try_from(failure.index)
                    .ok()
                    .and_then(|at| results.get_mut(at))
                    .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
                *slot = Some(Err(failure.body()));
            }
            Ok(WriteOutput::End(_)) => break,
            Ok(WriteOutput::Error(body)) => return Err(Error::Remote(body)),
            Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let all = results
        .into_iter()
        .map(|slot| slot.ok_or(Error::Closed))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((all, stats))
}
