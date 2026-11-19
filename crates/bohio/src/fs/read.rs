//! `bohio-read/1`: one file streamed to the client in chunks.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use bohio_proto::fs::{ReadHeader, ReadOutput, ReadRequest};
use bohio_proto::{ErrorBody, ErrorCode};
use coqui_transport::Channel;

use super::ops::{self, Roots};
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
/// of data bytes sent.
///
/// # Errors
///
/// The [`ErrorBody`] sent to the peer: `bad_request` for an undecodable request, a directory or an
/// offset past the end; `unsupported`, `denied` and `busy` (at `max_transfers`) before any path
/// is looked at; `outside_root`, `not_found` and `io` from the path and the file.
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
    authorize(context, &[Permission::FsRead])?;
    let _permit = context.slots.acquire().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Busy,
            "the host is running as many transfers as it allows",
        )
    })?;
    let roots = Roots::new(context.grant.fs_roots());
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

    let give_up = || ErrorBody::new(ErrorCode::Io, "the stream ended before the file was sent");
    let send = |output: &ReadOutput| send_waiting(channel.as_ref(), &encode(output), poll, idle);
    if !send(&ReadOutput::Header(ReadHeader { size, length })) {
        return Err(give_up());
    }
    let mut buffer = vec![0_u8; chunk_size(channel.as_ref(), context.config.chunk_size)];
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
        if !send(&ReadOutput::Data(buffer[..count].to_vec())) {
            return Err(give_up());
        }
        remaining -= count as u64;
        sent += count as u64;
    }
    if !send(&ReadOutput::Done) {
        return Err(give_up());
    }
    Ok(sent)
}
