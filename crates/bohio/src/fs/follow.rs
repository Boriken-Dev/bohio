//! `bohio-read/1` with `follow`: a file sent from where the read starts and then as it grows, until the client closes the
//! stream (`tail -f`).

use super::vfs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bohio_proto::fs::{FollowBy, ReadFollow, ReadHeader, ReadOutput, ReadRequest, Truncated};
use bohio_proto::{ErrorBody, ErrorCode};
use coqui_transport::Channel;

use super::bundle::grant_now;
use super::ops::{self, Roots};
use super::{Context, authorize, io_error};
use crate::policy::Permission;
use crate::stream::{chunk_size, send_waiting};

fn bad(message: &'static str) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

/// What tells one file from another that took its name: the device and inode on Unix, the volume serial number and file
/// index on Windows (its creation time is no help there: a file made under a name just freed is given the old one's).
type Identity = (Option<u64>, Option<u64>);

fn identity(meta: &Metadata) -> Identity {
    // Made from an open file, as `open_regular` makes it: on Windows the handle is what knows these.
    (meta.0.device, meta.0.inode)
}

/// Serves a followed read: the header (with `follow` true), then data frames as bytes arrive, `truncated` frames when
/// the file gets shorter or is replaced, until the client closes the stream. Returns the data bytes sent.
///
/// The follow holds one of `[filesystem] max_follows` slots and no transfer slot, and `idle_timeout` does not end it while
/// the client keeps the stream open and reads what is sent. With `by: name` the path is looked up again at every look
/// through the peer's root handle; a file that took the name is sent from its start after the old one has been read to its
/// end.
///
/// # Errors
///
/// The [`ErrorBody`] sent as the stream's `error` frame: `bad_request` for a request that also asks for `length`, hashes,
/// compression, `prepare`, `resume`, `delta` or `items`, for an interval of zero or an offset past the end; `denied`,
/// `busy` at `max_follows`, and the path's own errors.
pub(super) fn serve(
    channel: &Arc<dyn Channel>,
    request: &ReadRequest,
    follow: &ReadFollow,
    context: &Context<'_>,
    poll: Duration,
    idle: Duration,
) -> Result<u64, ErrorBody> {
    authorize(context, &[Permission::FsRead])?;
    let plain = request
        .compression
        .as_ref()
        .is_none_or(|compression| !compression.algorithm.compresses());
    if request.length.is_some()
        || !request.hashes.is_empty()
        || !plain
        || request.prepare
        || request.resume.is_some()
        || request.delta.is_some()
        || request.items
    {
        return Err(bad(
            "a followed read has no length, hashes, compression, prepare, resume, delta or items",
        ));
    }
    let interval = match follow.interval_ms {
        Some(0) => return Err(bad("a follow interval of zero looks at nothing")),
        Some(ms) => Duration::from_millis(ms),
        None => context.config.follow_interval.0,
    };
    let _permit = context.follows.acquire().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Busy,
            "the host is running as many followed reads as it allows",
        )
    })?;
    let roots = Roots::new(context.grant.fs_roots(), context.source.0);
    let (mut file, meta) = ops::open_regular(&roots, &request.path)?;
    let size = meta.len();
    let mut position = match follow.tail {
        Some(tail) => size.saturating_sub(tail),
        None if request.offset > size => {
            return Err(bad("the offset is past the end of the file"));
        }
        None => request.offset,
    };
    file.seek(SeekFrom::Start(position))
        .map_err(|error| io_error(&error))?;
    let mut tracked = identity(&meta);
    let by_name = follow.by == Some(FollowBy::Name);
    let chunk = chunk_size(channel.as_ref(), context.config.chunk_size);
    let give_up = || {
        ErrorBody::new(
            ErrorCode::Io,
            "the stream ended while the file was followed",
        )
    };
    let send = |output: &ReadOutput| {
        output
            .encode()
            .is_ok_and(|frame| send_waiting(channel.as_ref(), &frame, poll, idle))
    };
    let header = ReadHeader {
        follow: true,
        ..ReadHeader::raw(size, size - position)
    };
    if !send(&ReadOutput::Header(header)) {
        return Err(give_up());
    }
    let mut buffer = vec![0_u8; chunk];
    let mut sent = 0_u64;
    loop {
        sent +=
            drain(&mut file, &mut position, &mut buffer, &send).map_err(|error| match error {
                Drained::Gone => give_up(),
                Drained::Failed(body) => body,
            })?;
        // A shorter file: what the host had read is gone, so it goes on from the new end.
        let now = file.metadata().map_err(|error| io_error(&error))?.len();
        if now < position {
            position = now;
            file.seek(SeekFrom::Start(position))
                .map_err(|error| io_error(&error))?;
            if !send(&ReadOutput::Truncated(Truncated {
                size: now,
                replaced: false,
            })) {
                return Err(give_up());
            }
        }
        if by_name && let Some((next, meta)) = replacement(context, &request.path, &tracked)? {
            // The old file was read to its end above; the new one is sent from its start.
            file = next;
            tracked = identity(&meta);
            position = 0;
            if !send(&ReadOutput::Truncated(Truncated {
                size: meta.len(),
                replaced: true,
            })) {
                return Err(give_up());
            }
            continue;
        }
        if !wait(channel.as_ref(), interval, poll) {
            return Ok(sent);
        }
    }
}

enum Drained {
    /// The stream is gone.
    Gone,
    Failed(ErrorBody),
}

/// Sends what is in the file from `position` to its end now; returns the bytes sent.
fn drain(
    file: &mut File,
    position: &mut u64,
    buffer: &mut [u8],
    send: &dyn Fn(&ReadOutput) -> bool,
) -> Result<u64, Drained> {
    let mut sent = 0_u64;
    loop {
        let count = match file.read(buffer) {
            Ok(0) => return Ok(sent),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Drained::Failed(io_error(&error))),
        };
        if !send(&ReadOutput::Data(buffer[..count].to_vec())) {
            return Err(Drained::Gone);
        }
        *position += count as u64;
        sent += count as u64;
    }
}

/// The file now at `path` when it is not the one `tracked` names; `None` when it is the same, or there is none yet (the
/// name is free between a rotation's rename and the new file).
fn replacement(
    context: &Context<'_>,
    path: &str,
    tracked: &Identity,
) -> Result<Option<(File, Metadata)>, ErrorBody> {
    let roots = Roots::new(context.grant.fs_roots(), context.source.0);
    let (file, meta) = match ops::open_regular(&roots, path) {
        Ok(found) => found,
        Err(body) if matches!(body.code, ErrorCode::NotFound | ErrorCode::BadRequest) => {
            return Ok(None);
        }
        Err(body) => return Err(body),
    };
    if identity(&meta) == *tracked {
        return Ok(None);
    }
    // Another file took the name. It is opened under the grant as it is now: a root taken away ends the follow.
    let grant = grant_now(context)?;
    let files = Context {
        grant: &grant,
        ..*context
    };
    authorize(&files, &[Permission::FsRead])?;
    Roots::new(grant.fs_roots(), files.source.0).resolve_entry(path)?;
    Ok(Some((file, meta)))
}

/// Waits `interval`, looking at the stream every `poll`; `false` when the stream was closed.
fn wait(channel: &dyn Channel, interval: Duration, poll: Duration) -> bool {
    let until = Instant::now() + interval;
    loop {
        if channel.is_closed() {
            return false;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(poll));
    }
}
