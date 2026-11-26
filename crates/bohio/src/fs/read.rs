//! `bohio-read/1`: one file streamed to the client in chunks.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use bohio_proto::compress::Compression;
use bohio_proto::fs::{ReadDone, ReadHeader, ReadOutput, ReadRequest};
use bohio_proto::{ErrorBody, ErrorCode, compress::Progress};
use bohio_transfer::compress::{Encoder, Piece, ratio};
use coqui_transport::Channel;

use super::compress::Settings;
use super::delta;
use super::hash::hasher_for;
use super::ops::{self, Roots};
use super::prepare;
use super::{Context, authorize, io_error};
use crate::policy::Permission;
use crate::stream::{chunk_size, send_waiting};

fn encode(output: &ReadOutput) -> Vec<u8> {
    output.encode().unwrap_or_else(|_| vec![0x05])
}

/// Serves one `bohio-read/1` stream whose open carried `metadata`, until every byte is sent.
///
/// Frames out: `header`, then `data` frames in order, then `done`. Sending waits while the stream
/// has no room, so a slow client slows the read instead of growing a buffer; a client that makes
/// no progress for `idle_timeout`, or closes the stream, ends it. A request that cannot be served
/// gets one `error` frame and the same error is returned; the stream is left open for the caller
/// to close. A file that shrinks while it is read ends with an `io` error frame. Returns the number
/// of data bytes sent. With `hashes` in the request the closing `done` frame carries those digests
/// of the bytes sent.
///
/// With `delta` the client sends the signature of its old copy (`signature` frames, then `signature end`), and
/// the host answers a header with `delta` true, `copy` frames for the blocks of the old copy that stand in the
/// file and data (or compressed) frames for the rest, then `done`; the signature is bounded by `[delta]
/// max_signature_bytes` (`busy`) and the block size by `max_block`.
///
/// With `compression` each chunk is a `compressed` frame when that is smaller and a `data` frame
/// when it is not (which ends the compression context); the header says what was used, and a file
/// whose extension is in `[compression] skip_extensions`, whose name matches `exclude`, or whose
/// start does not compress (`sample_bytes`, `sample_ratio`) is sent raw. With `prepare` the range
/// is compressed into a file first (`progress` frames while it works) and the compressed bytes are
/// sent from the start, or from `resume`'s offset.
///
/// # Errors
///
/// The [`ErrorBody`] sent to the peer: `bad_request` for an undecodable request, a directory or an
/// offset past the end; `unsupported` (also for a digest or an algorithm the host does not offer),
/// `denied` and `busy` (at `max_transfers`, or a prepared result past `prepare_max_bytes`) before
/// any path is looked at; `outside_root`, `not_found` (also an unknown or expired token) and `io`
/// from the path and the file.
pub fn serve_read(
    channel: Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<u64, ErrorBody> {
    let poll = context.config.poll_interval.0;
    let idle = context.config.idle_timeout.0;
    match run(&channel, metadata, context, poll, idle) {
        Ok(sent) => Ok(sent),
        Err(error) => {
            // Best effort: a stream with no room for one more frame right now gets none.
            let frame = encode(&ReadOutput::Error(error.clone()));
            let _ = send_waiting(channel.as_ref(), &frame, poll, poll);
            Err(error)
        }
    }
}

fn run(
    channel: &Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
    poll: Duration,
    idle: Duration,
) -> Result<u64, ErrorBody> {
    let request = ReadRequest::from_metadata(metadata)
        .map_err(|_| ErrorBody::new(ErrorCode::BadRequest, "the request cannot be read"))?;
    if let Some(follow) = &request.follow {
        return super::follow::serve(channel, &request, follow, context, poll, idle);
    }
    if request.items {
        return super::bundle::read(channel, &request, context, poll, idle);
    }
    authorize(context, &[Permission::FsRead])?;
    let settings = Settings::new(context.compression);
    let resolved = settings.resolve(request.compression.as_ref())?;
    if request.prepare && resolved.is_none() && request.resume.is_none() {
        return Err(ErrorBody::new(
            ErrorCode::BadRequest,
            "a prepared read needs a compression to prepare with",
        ));
    }
    if request.delta.is_some() && (request.prepare || request.resume.is_some()) {
        return Err(ErrorBody::new(
            ErrorCode::BadRequest,
            "a delta read is neither prepared nor resumed",
        ));
    }
    let mut hasher = hasher_for(context, &request.hashes)?;
    let _permit = context.slots.acquire().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Busy,
            "the host is running as many transfers as it allows",
        )
    })?;
    let roots = Roots::new(context.grant.fs_roots(), context.source);
    let give_up = || ErrorBody::new(ErrorCode::Io, "the stream ended before the file was sent");
    let send = |output: &ReadOutput| send_waiting(channel.as_ref(), &encode(output), poll, idle);
    let chunk = chunk_size(channel.as_ref(), context.config.chunk_size);

    if let Some(resume) = &request.resume {
        let dir = prepared_dir(&settings)?;
        let prepared = prepare::open(
            dir,
            &resume.token,
            context.peer,
            context.compression.prepare_ttl.0,
        )?;
        if prepared.meta.path != request.path {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "the path is not the one the prepared read was made for",
            ));
        }
        // The grant may have narrowed since the file was made: its path must still be one the peer reads.
        roots.resolve(&prepared.meta.path)?;
        if resume.offset > prepared.meta.compressed_length {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "the offset is past the end of the prepared stream",
            ));
        }
        return send_prepared(prepared, resume.offset, chunk, &send, give_up);
    }

    let (mut file, meta) = ops::open_regular(&roots, &request.path)?;
    let size = meta.len();
    if request.offset > size {
        return Err(ErrorBody::new(
            ErrorCode::BadRequest,
            "the offset is past the end of the file",
        ));
    }
    let length = request.length.map_or(size - request.offset, |wanted| {
        wanted.min(size - request.offset)
    });
    file.seek(SeekFrom::Start(request.offset))
        .map_err(|error| io_error(&error))?;

    if let Some(options) = &request.delta {
        if request.offset != 0 || request.length.is_some() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "a delta read is of the whole file",
            ));
        }
        let job = delta::Pull {
            channel: channel.as_ref(),
            context,
            delta: options,
            settings: &settings,
            compression: resolved.as_ref(),
            size,
            chunk,
            poll,
            idle,
        };
        return delta::pull(&job, &mut file, hasher, settings.skips(&request.path));
    }

    if request.prepare {
        // A file `[compression] exclude` names is never compressed, so there is nothing to prepare: the size of the
        // compressed copy would show a path observer what the file holds.
        if settings.skips(&request.path) {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "a file `[compression] exclude` or `skip_extensions` names is not prepared compressed",
            ));
        }
        let compression = resolved.as_ref().ok_or_else(|| {
            ErrorBody::new(ErrorCode::BadRequest, "a prepared read needs a compression")
        })?;
        let dir = prepared_dir(&settings)?;
        prepare::sweep(dir, context.compression.prepare_ttl.0);
        let encoder = Encoder::new(compression, settings.caps())
            .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))?;
        let job = prepare::Job {
            dir,
            max_bytes: context.compression.prepare_max_bytes,
            peer: context.peer,
            path: &request.path,
            size,
            length,
            chunk,
            every: (idle / 4).max(Duration::from_millis(1)),
        };
        let prepared = prepare::make(&job, &mut file, encoder, hasher, &mut |prepared| {
            send(&ReadOutput::Progress(Progress { prepared }))
        })?;
        return send_prepared(prepared, 0, chunk, &send, give_up);
    }

    // Whether this file is compressed at all, and with what.
    let mut encoder = None;
    if let Some(compression) = &resolved
        && length > 0
        && !settings.skips(&request.path)
        && !sampled_as_raw(&settings, compression, &mut file, length, request.offset)?
    {
        encoder = Some(
            Encoder::new(compression, settings.caps())
                .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))?,
        );
    }
    let used = encoder.as_ref().map(|encoder| encoder.used().clone());
    let header = ReadHeader {
        compression: used,
        ..ReadHeader::raw(size, length)
    };
    if !send(&ReadOutput::Header(header)) {
        return Err(give_up());
    }
    let mut buffer = vec![0_u8; chunk];
    let mut remaining = length;
    let mut sent = 0_u64;
    while remaining > 0 {
        let want = usize::try_from(remaining).map_or(buffer.len(), |left| left.min(buffer.len()));
        let count = match file.read(&mut buffer[..want]) {
            Ok(0) => {
                return Err(ErrorBody::new(
                    ErrorCode::Io,
                    "the file shrank while it was read",
                ));
            }
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        };
        let frame = match encoder.as_mut() {
            Some(encoder) => match encoder
                .frame(&buffer[..count])
                .map_err(|error| ErrorBody::new(ErrorCode::Internal, error.to_string()))?
            {
                Piece::Compressed(piece) => ReadOutput::Compressed(piece),
                Piece::Raw => ReadOutput::Data(buffer[..count].to_vec()),
            },
            None => ReadOutput::Data(buffer[..count].to_vec()),
        };
        if !send(&frame) {
            return Err(give_up());
        }
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
        sent += count as u64;
    }
    let hashes = hasher.finish();
    if !send(&ReadOutput::Done(ReadDone {
        hashes,
        index: None,
    })) {
        return Err(give_up());
    }
    Ok(sent)
}

fn prepared_dir<'a>(settings: &Settings<'a>) -> Result<&'a std::path::Path, ErrorBody> {
    settings.config().prepare_dir.as_deref().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Unsupported,
            "this host does not prepare compressed reads",
        )
    })
}

/// Whether the start of the range compresses so badly (`sample_ratio`) that the file is sent raw. The
/// file is left where it was.
pub(super) fn sampled_as_raw(
    settings: &Settings<'_>,
    compression: &Compression,
    file: &mut (impl Read + Seek),
    length: u64,
    offset: u64,
) -> Result<bool, ErrorBody> {
    let config = settings.config();
    if config.sample_bytes == 0 {
        return Ok(false);
    }
    let want =
        usize::try_from(length).map_or(config.sample_bytes, |all| all.min(config.sample_bytes));
    let mut sample = vec![0_u8; want];
    file.read_exact(&mut sample)
        .map_err(|error| io_error(&error))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| io_error(&error))?;
    let measured = ratio(compression, settings.caps(), &sample)
        .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))?;
    Ok(measured > config.sample_ratio)
}

/// Sends a prepared file from `offset`: the header, the compressed bytes and the digests.
fn send_prepared(
    prepared: prepare::Prepared,
    offset: u64,
    chunk: usize,
    send: &dyn Fn(&ReadOutput) -> bool,
    give_up: impl Fn() -> ErrorBody,
) -> Result<u64, ErrorBody> {
    let prepare::Prepared {
        token,
        meta,
        mut file,
    } = prepared;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| io_error(&error))?;
    let header = ReadHeader {
        size: meta.size,
        length: meta.length,
        compression: Some(meta.compression.clone()),
        compressed_length: Some(meta.compressed_length),
        token: Some(token),
        ..ReadHeader::raw(meta.size, meta.length)
    };
    if !send(&ReadOutput::Header(header)) {
        return Err(give_up());
    }
    let mut buffer = vec![0_u8; chunk];
    let mut sent = offset;
    while sent < meta.compressed_length {
        let want = usize::try_from(meta.compressed_length - sent)
            .map_or(buffer.len(), |left| left.min(buffer.len()));
        let count = match file.read(&mut buffer[..want]) {
            Ok(0) => {
                return Err(ErrorBody::new(
                    ErrorCode::Io,
                    "the prepared file is shorter than it was made",
                ));
            }
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        };
        if !send(&ReadOutput::Compressed(buffer[..count].to_vec())) {
            return Err(give_up());
        }
        sent += count as u64;
    }
    if !send(&ReadOutput::Done(ReadDone {
        hashes: meta.hashes,
        index: None,
    })) {
        return Err(give_up());
    }
    Ok(meta.length)
}
