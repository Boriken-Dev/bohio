//! Bundles: many files on one `bohio-read/1` or `bohio-write/1` stream, so a tree of small files costs one open
//! and one compression context instead of one each.
//!
//! Each item is decided by the peer's grant as it is when the item is reached (`Context::current`), and fails
//! alone: the stream goes on with the next one.

use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bohio_proto::compress::Compression;
use bohio_proto::fs::{
    BundleEnd, ReadDone, ReadHeader, ReadInput, ReadItem, ReadOutput, ReadRequest, WriteDone,
    WriteInput, WriteItem, WriteMode, WriteOutput, WriteRequest,
};
use bohio_proto::hash::{Algorithm, Digests, ItemError};
use bohio_proto::{ErrorBody, ErrorCode};
use bohio_transfer::compress::{Encoder, Piece};
use bohio_transfer::hash::Hasher;
use coqui_transport::{Channel, Failure};

use super::compress::{Settings, Sink, decode_failure};
use super::hash::hasher_for;
use super::ops::{self, Roots};
use super::read::sampled_as_raw;
use super::stage::Staged;
use super::{Context, authorize, io_error};
use crate::policy::{Grant, Permission};
use crate::stream::{chunk_size, send_waiting};

/// The peer's grant as it is now.
pub(super) fn grant_now(context: &Context<'_>) -> Result<Grant, ErrorBody> {
    match context.current {
        Some(current) => (current.0)(),
        None => Ok(context.grant.clone()),
    }
}

fn bad(message: &'static str) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

fn too_many(limit: u64) -> bool {
    limit != 0
}

fn ended() -> ErrorBody {
    ErrorBody::new(ErrorCode::Io, "the stream ended before the items were done")
}

/// The bundle a read opened: serves `request.items` until the client ends its items. Returns the data bytes sent.
pub(super) fn read(
    channel: &Arc<dyn Channel>,
    request: &ReadRequest,
    context: &Context<'_>,
    poll: Duration,
    idle: Duration,
) -> Result<u64, ErrorBody> {
    authorize(context, &[Permission::FsRead])?;
    if request.prepare || request.resume.is_some() || request.delta.is_some() {
        return Err(bad("a bundle is neither prepared, resumed nor a delta"));
    }
    let settings = Settings::new(context.compression);
    let resolved = settings.resolve(request.compression.as_ref())?;
    // The algorithms are checked once; each item makes its own hasher.
    hasher_for(context, &request.hashes)?;
    let _permit = context.slots.acquire().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Busy,
            "the host is running as many transfers as it allows",
        )
    })?;
    let mut encoder = resolved
        .as_ref()
        .map(|compression| Encoder::new(compression, settings.caps()))
        .transpose()
        .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))?;
    let first = ReadHeader {
        compression: encoder.as_ref().map(|encoder| encoder.used().clone()),
        ..ReadHeader::raw(0, 0)
    };
    let send = |output: &ReadOutput| {
        send_waiting(
            channel.as_ref(),
            &output.encode().unwrap_or_else(|_| vec![0x05]),
            poll,
            idle,
        )
    };
    if !send(&ReadOutput::Header(first)) {
        return Err(ended());
    }
    let chunk = chunk_size(channel.as_ref(), context.config.chunk_size);
    let mut index = 0_u64;
    let mut sent = 0_u64;
    let mut last_progress = Instant::now();
    loop {
        match channel.receive(Some(poll)) {
            Ok(Some(frame)) => match ReadInput::decode(&frame) {
                Ok(ReadInput::Item(item)) => {
                    if too_many(context.config.max_bundle_items)
                        && index >= context.config.max_bundle_items
                    {
                        return Err(ErrorBody::new(
                            ErrorCode::Busy,
                            "the bundle holds more files than [filesystem] max_bundle_items allows",
                        ));
                    }
                    let job = ReadJob {
                        channel: channel.as_ref(),
                        context,
                        settings: &settings,
                        compression: resolved.as_ref(),
                        hashes: &request.hashes,
                        chunk,
                        index,
                    };
                    match read_item(&job, &item, encoder.as_mut(), &send) {
                        Ok(bytes) => sent += bytes,
                        Err(ItemFailure::Item(body)) => {
                            if !send(&ReadOutput::ItemError(ItemError::new(index, body))) {
                                return Err(ended());
                            }
                        }
                        Err(ItemFailure::Stream(body)) => return Err(body),
                    }
                    index += 1;
                    last_progress = Instant::now();
                }
                Ok(ReadInput::ItemsEnd) => break,
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
    if !send(&ReadOutput::End(BundleEnd { items: index })) {
        return Err(ended());
    }
    Ok(sent)
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

struct ReadJob<'a> {
    channel: &'a dyn Channel,
    context: &'a Context<'a>,
    settings: &'a Settings<'a>,
    compression: Option<&'a Compression>,
    hashes: &'a [Algorithm],
    chunk: usize,
    index: u64,
}

fn read_item(
    job: &ReadJob<'_>,
    item: &ReadItem,
    mut encoder: Option<&mut Encoder>,
    send: &dyn Fn(&ReadOutput) -> bool,
) -> Result<u64, ItemFailure> {
    let grant = grant_now(job.context).map_err(ItemFailure::Stream)?;
    let files = Context {
        grant: &grant,
        ..*job.context
    };
    authorize(&files, &[Permission::FsRead])?;
    let roots = Roots::new(grant.fs_roots());
    let (mut file, meta) = ops::open_regular(&roots, &item.path)?;
    let size = meta.len();
    if item.offset > size {
        return Err(bad("the offset is past the end of the file").into());
    }
    let length = item
        .length
        .map_or(size - item.offset, |wanted| wanted.min(size - item.offset));
    file.seek(SeekFrom::Start(item.offset))
        .map_err(|error| io_error(&error))?;
    let mut hasher = hasher_for(&files, job.hashes)?;
    // This file goes compressed unless its name or its start says it would not help; the context then
    // carries on from the item before.
    let compressed = match job.compression {
        Some(compression)
            if length > 0
                && !job.settings.skips(&item.path)
                && !sampled_as_raw(job.settings, compression, &mut file, length, item.offset)? =>
        {
            encoder.is_some()
        }
        _ => false,
    };
    #[cfg(unix)]
    let mode = {
        use cap_std::fs::PermissionsExt;
        Some(meta.permissions().mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let mode = None;
    let header = ReadHeader {
        index: Some(job.index),
        modified: meta.modified().ok().map(ops::timestamp),
        mode,
        ..ReadHeader::raw(size, length)
    };
    if !send(&ReadOutput::Header(header)) {
        return Err(ItemFailure::Stream(ended()));
    }
    let mut buffer = vec![0_u8; job.chunk];
    let mut remaining = length;
    let mut sent_raw = false;
    while remaining > 0 {
        if job.channel.is_closed() {
            return Err(ItemFailure::Stream(ended()));
        }
        let want = usize::try_from(remaining).map_or(buffer.len(), |left| left.min(buffer.len()));
        let count = match file.read(&mut buffer[..want]) {
            Ok(0) => {
                return Err(
                    ErrorBody::new(ErrorCode::Io, "the file shrank while it was read").into(),
                );
            }
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error).into()),
        };
        let frame = match encoder.as_deref_mut().filter(|_| compressed) {
            Some(encoder) => match encoder
                .frame(&buffer[..count])
                .map_err(|error| ErrorBody::new(ErrorCode::Internal, error.to_string()))?
            {
                Piece::Compressed(piece) => ReadOutput::Compressed(piece),
                Piece::Raw => ReadOutput::Data(buffer[..count].to_vec()),
            },
            None => {
                sent_raw = true;
                ReadOutput::Data(buffer[..count].to_vec())
            }
        };
        if !send(&frame) {
            return Err(ItemFailure::Stream(ended()));
        }
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    // A raw frame ended the client's context, so the next compressed piece must start a new one.
    if sent_raw && let Some(encoder) = encoder {
        encoder.restart();
    }
    let done = ReadDone {
        hashes: hasher.finish(),
        index: Some(job.index),
    };
    if !send(&ReadOutput::Done(done)) {
        return Err(ItemFailure::Stream(ended()));
    }
    Ok(length)
}

/// A bundle a write opened: serves `request.items` until the client ends its items.
pub(super) fn write(
    channel: &Arc<dyn Channel>,
    request: &WriteRequest,
    context: &Context<'_>,
    poll: Duration,
) -> Result<WriteDone, ErrorBody> {
    let idle = context.config.idle_timeout.0;
    authorize(context, &[Permission::FsWrite])?;
    super::write::check_request(request)?;
    let settings = Settings::new(context.compression);
    let declared = settings.declared(request.compression.as_ref())?;
    let mut decoder = declared
        .as_ref()
        .map(|used| settings.decoder(used))
        .transpose()?;
    let answered: Vec<Algorithm> = request
        .hashes
        .clone()
        .unwrap_or_else(|| context.config.write_hashes.clone());
    hasher_for(context, &answered)?;
    let _permit = context.slots.acquire().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Busy,
            "the host is running as many transfers as it allows",
        )
    })?;
    let send = |output: &WriteOutput| {
        send_waiting(
            channel.as_ref(),
            &output.encode().unwrap_or_else(|_| vec![0x05]),
            poll,
            idle,
        )
    };
    if !send(&WriteOutput::ready()) {
        return Err(ended());
    }
    let mut index = 0_u64;
    let mut total = 0_u64;
    let mut open: Option<Open> = None;
    let mut last_progress = Instant::now();
    loop {
        match channel.receive(Some(poll)) {
            Ok(Some(frame)) => match WriteInput::decode(&frame) {
                Ok(WriteInput::Item(item)) => {
                    if open.is_some() {
                        return Err(bad("an item began before the end of the one before it"));
                    }
                    if too_many(context.config.max_bundle_items)
                        && index >= context.config.max_bundle_items
                    {
                        return Err(ErrorBody::new(
                            ErrorCode::Busy,
                            "the bundle holds more files than [filesystem] max_bundle_items allows",
                        ));
                    }
                    open = Some(begin_item(context, &item, &answered, index));
                    last_progress = Instant::now();
                }
                Ok(WriteInput::Data(bytes)) => {
                    let Some(item) = open.as_mut() else {
                        return Err(bad("data came outside an item"));
                    };
                    // A raw frame ends the compression context.
                    if let Some(decoder) = decoder.as_mut() {
                        decoder.reset();
                    }
                    item.take_raw(&bytes);
                    last_progress = Instant::now();
                }
                Ok(WriteInput::Compressed(piece)) => {
                    let Some(item) = open.as_mut() else {
                        return Err(bad("data came outside an item"));
                    };
                    let decoder = decoder
                        .as_mut()
                        .ok_or_else(|| bad("compressed data needs a compression in the request"))?;
                    item.take_compressed(decoder, &piece)?;
                    last_progress = Instant::now();
                }
                Ok(WriteInput::End) => {
                    let Some(item) = open.take() else {
                        return Err(bad("an end came outside an item"));
                    };
                    let item_index = item.index;
                    let output = match item.finish(context, &answered) {
                        Ok(done) => {
                            total += done.written;
                            WriteOutput::Done(done)
                        }
                        Err(body) => WriteOutput::ItemError(ItemError::new(item_index, body)),
                    };
                    if !send(&output) {
                        return Err(ended());
                    }
                    index += 1;
                    last_progress = Instant::now();
                }
                Ok(WriteInput::ItemsEnd) => {
                    if open.is_some() {
                        return Err(bad("the items ended inside an item"));
                    }
                    break;
                }
                Ok(WriteInput::Copy(_) | WriteInput::Flush) => {
                    return Err(bad("a bundle takes no copy or flush frame"));
                }
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
                        "the client sent nothing for too long",
                    ));
                }
            }
            Ok(None) | Err(_) => return Err(ended()),
        }
    }
    if !send(&WriteOutput::End(BundleEnd { items: index })) {
        return Err(ended());
    }
    Ok(WriteDone {
        written: total,
        hashes: Digests::new(),
        index: None,
    })
}

/// The item being received: where its bytes go, or why they go nowhere.
struct Open {
    index: u64,
    /// `Err` when the item could not be opened or a write failed: the rest of its data is read and dropped.
    target: Result<Target, ErrorBody>,
}

struct Target {
    staged: Staged,
    hasher: Hasher,
    written: u64,
    expect: Digests,
    modified: Option<bohio_proto::fs::Timestamp>,
}

fn begin_item(context: &Context<'_>, item: &WriteItem, answered: &[Algorithm], index: u64) -> Open {
    Open {
        index,
        target: open_target(context, item, answered),
    }
}

fn open_target(
    context: &Context<'_>,
    item: &WriteItem,
    answered: &[Algorithm],
) -> Result<Target, ErrorBody> {
    let grant = grant_now(context)?;
    let files = Context {
        grant: &grant,
        ..*context
    };
    authorize(&files, &[Permission::FsWrite])?;
    if !matches!(item.mode, WriteMode::Replace | WriteMode::Create) {
        return Err(bad("a bundle's item is replace or create"));
    }
    if let Some(bits) = item.file_mode {
        super::check_mode(bits)?;
    }
    super::write::check_digests(&item.expect)?;
    let mut computed = answered.to_vec();
    for algorithm in item.expect.keys() {
        if !computed.contains(algorithm) {
            computed.push(algorithm.clone());
        }
    }
    let hasher = hasher_for(&files, &computed)?;
    let roots = Roots::new(grant.fs_roots());
    let (dir, relative) = roots.resolve(&item.path)?;
    let staged = Staged::open(&dir, &relative, &item.mode, item.parents, item.file_mode)?;
    Ok(Target {
        staged,
        hasher,
        written: 0,
        expect: item.expect.clone(),
        modified: item.modified,
    })
}

impl Open {
    fn take_raw(&mut self, bytes: &[u8]) {
        let Ok(target) = &mut self.target else { return };
        if let Err(error) = target.staged.file().write_all(bytes) {
            self.target = Err(io_error(&error));
            return;
        }
        target.hasher.update(bytes);
        target.written += bytes.len() as u64;
    }

    /// Decodes a compressed piece into the item, or into nothing when the item already failed (the decoder's
    /// context spans the items, so it must see every piece).
    fn take_compressed(
        &mut self,
        decoder: &mut bohio_transfer::compress::Decoder,
        piece: &[u8],
    ) -> Result<(), ErrorBody> {
        match &mut self.target {
            Ok(target) => {
                let mut sink = Sink {
                    out: target.staged.file(),
                    hasher: &mut target.hasher,
                    written: &mut target.written,
                };
                if let Err(error) = decoder.feed(piece, &mut sink) {
                    return Err(decode_failure(&error));
                }
            }
            Err(_) => {
                decoder
                    .feed(piece, &mut std::io::sink())
                    .map_err(|error| decode_failure(&error))?;
            }
        }
        Ok(())
    }

    fn finish(self, context: &Context<'_>, answered: &[Algorithm]) -> Result<WriteDone, ErrorBody> {
        let mut target = self.target?;
        let mut digests = target.hasher.finish();
        if let Some(name) = target
            .expect
            .iter()
            .find(|(algorithm, want)| digests.get(*algorithm) != Some(*want))
            .map(|(algorithm, _)| algorithm)
        {
            return Err(ErrorBody::new(
                ErrorCode::Io,
                format!("the data does not match the expected {name}"),
            ));
        }
        digests.retain(|algorithm, _| answered.contains(algorithm));
        if let Some(stamp) = target.modified {
            target.staged.set_modified(ops::system_time(stamp)?);
        }
        target.staged.commit(context.config.sync_writes)?;
        Ok(WriteDone {
            written: target.written,
            hashes: digests,
            index: Some(self.index),
        })
    }
}
