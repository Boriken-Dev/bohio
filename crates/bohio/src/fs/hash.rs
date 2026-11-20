//! `bohio-hash/1`: digests of many files on one stream, and the digest helper reads and writes share.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bohio_proto::fs::Timestamp;
use bohio_proto::hash::{
    Algorithm, HashDone, HashInput, HashItem, HashOutput, HashRequest, HashResult, ItemError,
};
use bohio_proto::{ErrorBody, ErrorCode};
use bohio_transfer::hash::Hasher;
use coqui_transport::{Channel, Failure};

use super::ops::{self, Roots};
use super::{Context, authorize, io_error};
use crate::control::Current;
use crate::limit::Slots;
use crate::policy::{Grant, Permission};
use crate::stream::send_waiting;

/// What the hash engine needs besides the stream.
#[derive(Debug, Clone, Copy)]
pub struct HashContext<'a> {
    /// The filesystem engines' context: the peer's grant, `[filesystem]` and `[hash]`.
    pub files: Context<'a>,
    /// The bound on concurrent hash streams, shared by every stream.
    pub slots: &'a Slots,
    /// Where the peer's grant is read before each item, so a grant narrowed while the stream is open
    /// applies to the next item; `files.grant` is used when absent.
    pub current: Option<Current<'a>>,
}

/// A hasher for `wanted`, when the host allows each algorithm (`[hash] algorithms`).
///
/// # Errors
///
/// `unsupported` for an algorithm this host does not offer, named in the message.
pub(crate) fn hasher_for(context: &Context<'_>, wanted: &[Algorithm]) -> Result<Hasher, ErrorBody> {
    allowed(context, wanted)?;
    Hasher::new(wanted).map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))
}

/// Checks that the host offers every algorithm of `wanted`.
pub(crate) fn allowed(context: &Context<'_>, wanted: &[Algorithm]) -> Result<(), ErrorBody> {
    match wanted
        .iter()
        .find(|name| !context.hash.algorithms.contains(name))
    {
        Some(name) => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!("this host does not offer {name}"),
        )),
        None => Ok(()),
    }
}

fn encode(output: &HashOutput) -> Vec<u8> {
    output.encode().unwrap_or_else(|_| vec![0x05])
}

/// Serves one `bohio-hash/1` stream whose open carried `metadata`, until the client ends its items.
///
/// Frames in: one `item` per file, then `end`. Frames out: for each item, in order, a `result` or an
/// `item error` (the stream goes on), then `done` with the count. Each item's path is resolved against
/// the peer's grant as it is when the item is reached, so a grant narrowed while the stream is open
/// refuses the next item (`outside_root` or `denied`); an item whose `if_size` and `if_modified` both
/// match the file is answered `unchanged` and nothing is read. A stream that gets no item for
/// `idle_timeout`, or whose client closes it, ends; a client that stops reading results ends it the
/// same way. With `[hash] rate` a stream reads at most that many bytes a second. Returns how many items
/// were answered.
///
/// # Errors
///
/// The [`ErrorBody`] sent as the stream's `error` frame: `bad_request` for an undecodable request or
/// no algorithm, `unsupported` for an algorithm the host does not offer or for block hashes, `denied`
/// before any item, and `busy` at `[hash] max_streams`.
pub fn serve_hash(
    channel: Arc<dyn Channel>,
    metadata: &[u8],
    context: &HashContext<'_>,
) -> Result<u64, ErrorBody> {
    let poll = context.files.config.poll_interval.0;
    match run(&channel, metadata, context) {
        Ok(items) => Ok(items),
        Err(error) => {
            // Best effort: a stream with no room for one more frame right now gets none.
            let frame = encode(&HashOutput::Error(error.clone()));
            let _ = send_waiting(channel.as_ref(), &frame, poll, poll);
            Err(error)
        }
    }
}

fn run(
    channel: &Arc<dyn Channel>,
    metadata: &[u8],
    context: &HashContext<'_>,
) -> Result<u64, ErrorBody> {
    let files = &context.files;
    let poll = files.config.poll_interval.0;
    let idle = files.config.idle_timeout.0;
    let request = HashRequest::from_metadata(metadata)
        .map_err(|_| ErrorBody::new(ErrorCode::BadRequest, "the request cannot be read"))?;
    authorize(files, &[Permission::FsRead])?;
    if request.algorithms.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::BadRequest,
            "name at least one algorithm",
        ));
    }
    if request.blocks.is_some() {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "this host does not serve block hashes",
        ));
    }
    allowed(files, &request.algorithms)?;
    Hasher::new(&request.algorithms)
        .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))?;
    let _permit = context.slots.acquire().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Busy,
            "the host is running as many hash streams as it allows",
        )
    })?;
    let ended = || ErrorBody::new(ErrorCode::Io, "the stream ended before the items were done");

    let mut pace = Pace::new(files.hash.rate);
    let mut index = 0_u64;
    let mut last_progress = Instant::now();
    loop {
        match channel.receive(Some(poll)) {
            Ok(Some(frame)) => match HashInput::decode(&frame) {
                Ok(HashInput::Item(item)) => {
                    let output = match item_result(
                        channel.as_ref(),
                        context,
                        &request.algorithms,
                        &item,
                        index,
                        &mut pace,
                    ) {
                        Ok(result) => HashOutput::Result(result),
                        Err(ItemFailure::Item(body)) => {
                            HashOutput::ItemError(ItemError::new(index, body))
                        }
                        Err(ItemFailure::Stream(body)) => return Err(body),
                    };
                    if !send_waiting(channel.as_ref(), &encode(&output), poll, idle) {
                        return Err(ended());
                    }
                    index += 1;
                    last_progress = Instant::now();
                }
                Ok(HashInput::End) => break,
                // A frame this version does not know is skipped.
                Ok(_) | Err(_) => {}
            },
            Err(Failure::Timeout) => {
                if channel.is_closed() {
                    return Err(ended());
                }
                if last_progress.elapsed() >= idle {
                    return Err(ErrorBody::new(
                        ErrorCode::Io,
                        "the client sent no item for too long",
                    ));
                }
            }
            Ok(None) | Err(_) => return Err(ended()),
        }
    }
    let done = encode(&HashOutput::Done(HashDone { items: index }));
    if !send_waiting(channel.as_ref(), &done, poll, idle) {
        return Err(ended());
    }
    Ok(index)
}

/// Why an item produced no result: the item alone failed, or the stream cannot go on.
enum ItemFailure {
    Item(ErrorBody),
    Stream(ErrorBody),
}

impl From<ErrorBody> for ItemFailure {
    fn from(body: ErrorBody) -> Self {
        Self::Item(body)
    }
}

fn item_result(
    channel: &dyn Channel,
    context: &HashContext<'_>,
    algorithms: &[Algorithm],
    item: &HashItem,
    index: u64,
    pace: &mut Pace,
) -> Result<HashResult, ItemFailure> {
    let grant: Grant;
    let files = if let Some(current) = context.current {
        grant = (current.0)().map_err(ItemFailure::Stream)?;
        Context {
            grant: &grant,
            ..context.files
        }
    } else {
        context.files
    };
    authorize(&files, &[Permission::FsRead])?;
    let roots = Roots::new(files.grant.fs_roots());
    let (mut file, meta) = ops::open_regular(&roots, &item.path)?;
    let size = meta.len();
    let modified = meta.modified().ok().map(ops::timestamp);
    let same = |wanted: Option<u64>, stamp: Option<Timestamp>| {
        wanted == Some(size) && stamp.is_some() && stamp == modified
    };
    if same(item.if_size, item.if_modified) {
        return Ok(HashResult {
            index,
            size,
            modified,
            length: 0,
            unchanged: true,
            hashes: bohio_proto::hash::Digests::new(),
        });
    }
    if item.offset > size {
        return Err(ErrorBody::new(
            ErrorCode::BadRequest,
            "the offset is past the end of the file",
        )
        .into());
    }
    let length = item
        .length
        .map_or(size - item.offset, |wanted| wanted.min(size - item.offset));
    file.seek(SeekFrom::Start(item.offset))
        .map_err(|error| io_error(&error))?;
    let mut hasher = Hasher::new(algorithms)
        .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))?;
    let piece = pace.piece(files.hash.read_size);
    let mut buffer = vec![0_u8; piece];
    let mut remaining = length;
    while remaining > 0 {
        if channel.is_closed() {
            return Err(ItemFailure::Stream(ErrorBody::new(
                ErrorCode::Io,
                "the stream ended before the items were done",
            )));
        }
        let want = usize::try_from(remaining).map_or(buffer.len(), |left| left.min(buffer.len()));
        let count = match file.read(&mut buffer[..want]) {
            Ok(0) => {
                return Err(
                    ErrorBody::new(ErrorCode::Io, "the file shrank while it was hashed").into(),
                );
            }
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error).into()),
        };
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
        pace.spent(channel, count as u64);
    }
    Ok(HashResult {
        index,
        size,
        modified,
        length,
        unchanged: false,
        hashes: hasher.finish(),
    })
}

/// Holds a stream to `[hash] rate` bytes a second: each read is followed by a wait until its share of
/// time has passed. A wait that overshoots is made up by the next ones, up to a twentieth of a second, and
/// time a stream spent idle earns no more credit than that.
struct Pace {
    rate: u64,
    due: Instant,
}

/// The granularity of the limit: a read is at most this much of the rate, and this much lag is caught up.
const SLICE: Duration = Duration::from_millis(50);

impl Pace {
    fn new(rate: u64) -> Self {
        Self {
            rate,
            due: Instant::now(),
        }
    }

    /// The buffer size: `read_size`, or a twentieth of a second of `rate` when that is less.
    fn piece(&self, read_size: usize) -> usize {
        if self.rate == 0 {
            return read_size;
        }
        usize::try_from(self.rate / 20)
            .map_or(read_size, |slice| read_size.min(slice))
            .max(1)
    }

    /// Accounts for `count` bytes just read and waits until the rate allows the next read. The wait is at most a
    /// `SLICE`, so a stream that closes meanwhile is noticed at the next read.
    fn spent(&mut self, channel: &dyn Channel, count: u64) {
        if self.rate == 0 {
            return;
        }
        let now = Instant::now();
        let cost = Duration::from_secs_f64(count as f64 / self.rate as f64);
        let floor = now.checked_sub(SLICE).unwrap_or(now);
        self.due = self.due.max(floor) + cost;
        if let Some(left) = self.due.checked_duration_since(Instant::now())
            && !channel.is_closed()
        {
            std::thread::sleep(left);
        }
    }
}
