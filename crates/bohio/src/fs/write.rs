//! `bohio-write/1`: one file received from the client in chunks.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bohio_proto::fs::{
    DeltaBase, Flushed, Ready, Timestamp, WriteDone, WriteInput, WriteMode, WriteOutput,
    WriteRequest,
};
use bohio_proto::hash::Algorithm;
use bohio_proto::{ErrorBody, ErrorCode};
use bohio_transfer::apply::{apply, copy_len};
use bohio_transfer::compress::FREE_OUTPUT;
use bohio_transfer::delta::Op;
use coqui_transport::{Channel, Failure};

use super::compress::{Settings, Sink, decode_failure};
use super::hash::hasher_for;
use super::ops::{self, Roots};
use super::stage::Staged;
use super::{Context, authorize, io_error};
use crate::policy::Permission;
use crate::stream::send_waiting;

fn encode(output: &WriteOutput) -> Vec<u8> {
    output.encode().unwrap_or_else(|_| vec![0x05])
}

/// Serves one `bohio-write/1` stream whose open carried `metadata`, until the client ends the data
/// and it is in place.
///
/// Frames out: `ready` once the target is open, then `done` with the byte count and the digests the
/// request asked for (`hashes`, else `[filesystem] write_hashes`). For
/// `replace` the data goes to a temporary sibling and is renamed over the target only after the
/// client's `end` (and every digest in `expect` matched); a stream that closes, fails
/// or stays idle for `idle_timeout` first leaves the target as it was and removes the temporary
/// file. `create` removes the file it made. `append` and `at` write in place, so what was written
/// before the stream ended stays. A request that cannot be served gets one `error` frame and the
/// same error is returned; the stream is left open for the caller to close.
///
/// # Errors
///
/// The [`ErrorBody`] sent to the peer: `bad_request` for an undecodable request, an unusable mode
/// or digest text, a directory target or the root itself; `unsupported` (also for a digest the host
/// does not offer), `denied` and `busy` (at `max_transfers`) before any path is looked at; `outside_root`, `not_found`, `exists` (`create`)
/// and `io` from the path and the file, and `io` for a digest mismatch or a stream that ended
/// early.
///
/// A `delta` write builds the new file in a temporary sibling from `copy` frames (blocks of the target, read
/// through a handle opened before anything is written) and data frames, and renames it over the target as
/// `replace` does. It needs `fs.read` as well as `fs.write` (`denied`), and the target must be as the request's
/// `base` says (its size, its modification time and any digests): else the write is `changed` before `ready`, and
/// again just before the rename. A copy past the end of the target is `bad_request`, as is an output that
/// grows past `[delta] max_expansion` times the target's size plus what was received.
pub fn serve_write(
    channel: Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<WriteDone, ErrorBody> {
    let poll = context.config.poll_interval.0;
    match run(&channel, metadata, context, poll) {
        Ok(done) => Ok(done),
        Err(error) => {
            // Best effort: a stream with no room for one more frame right now gets none.
            let frame = encode(&WriteOutput::Error(error.clone()));
            let _ = send_waiting(channel.as_ref(), &frame, poll, poll);
            Err(error)
        }
    }
}

/// What names a kept temporary file besides its token: the peer and the path, so only the write that began it can
/// continue it.
fn resume_tag(peer: &str, path: &str) -> String {
    let mut hasher =
        bohio_transfer::hash::Hasher::new(&[Algorithm::Sha256]).expect("sha256 is always built");
    hasher.update(peer.as_bytes());
    hasher.update(&[0]);
    hasher.update(path.as_bytes());
    hasher
        .finish()
        .remove(&Algorithm::Sha256)
        .map_or_else(String::new, |digest| digest.chars().take(16).collect())
}

/// Checks that each expected digest is written as its algorithm writes it. An algorithm this build does not
/// know is refused as unsupported, with the other names, once the hasher is made.
pub(super) fn check_digests(expect: &bohio_proto::hash::Digests) -> Result<(), ErrorBody> {
    for (algorithm, expected) in expect {
        let text = algorithm
            .hex_len()
            .is_none_or(|length| length == expected.len())
            && expected
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !text {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "an expected digest is the algorithm's lowercase hexadecimal digits",
            ));
        }
    }
    Ok(())
}

pub(super) fn check_request(request: &WriteRequest) -> Result<(), ErrorBody> {
    let bad = |message: &'static str| Err(ErrorBody::new(ErrorCode::BadRequest, message));
    if let Some(bits) = request.file_mode {
        super::check_mode(bits)?;
    }
    check_digests(&request.expect)?;
    if !request.expect.is_empty()
        && !matches!(
            request.mode,
            WriteMode::Replace
                | WriteMode::Create
                | WriteMode::Delta { .. }
                | WriteMode::Resume { .. }
        )
    {
        return bad("a digest is only checked for replace, create, delta and resume");
    }
    if request.resume && !matches!(request.mode, WriteMode::Replace) {
        return bad(
            "only a replace asks to resume; a write that continues one names its token as the mode",
        );
    }
    if request.items
        && (request.resume
            || !request.expect.is_empty()
            || !matches!(request.mode, WriteMode::Replace)
            || request.file_mode.is_some()
            || request.parents)
    {
        return bad(
            "a bundle takes each file's mode, parents, file mode and digests from its items",
        );
    }
    if let WriteMode::Delta { block_size, base } = &request.mode {
        if *block_size == 0 || u32::try_from(*block_size).is_err() {
            return bad("a delta's block size is from 1 to 4294967295");
        }
        if base.hashes.iter().any(|(algorithm, text)| {
            algorithm
                .hex_len()
                .is_none_or(|length| length != text.len())
                || !text
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return bad("a base digest is the algorithm's lowercase hexadecimal digits");
        }
    }
    Ok(())
}

/// The target a delta write copies from, opened before anything is written.
struct Base {
    file: cap_std::fs::File,
    length: u64,
    modified: Option<Timestamp>,
    block_size: u32,
}

/// Opens the target of a delta write and holds it to what the client saw.
fn open_base(
    roots: &Roots<'_>,
    context: &Context<'_>,
    path: &str,
    block_size: u64,
    wanted: &DeltaBase,
) -> Result<Base, ErrorBody> {
    let block_size = u32::try_from(block_size)
        .ok()
        .filter(|size| (1..=context.delta.max_block).contains(size))
        .ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::BadRequest,
                "the block size is from 1 to this host's [delta] max_block",
            )
        })?;
    let (dir, relative) = roots.resolve(path)?;
    let (mut file, meta) = ops::open_regular_in(&dir, &relative)?;
    let modified = meta.modified().ok().map(ops::timestamp);
    let moved = || ErrorBody::new(ErrorCode::Changed, "the target is not as the client saw it");
    if meta.len() != wanted.size || (wanted.modified.is_some() && wanted.modified != modified) {
        return Err(moved());
    }
    if !wanted.hashes.is_empty() {
        let names: Vec<Algorithm> = wanted.hashes.keys().cloned().collect();
        let mut hasher = hasher_for(context, &names)?;
        let mut buffer = vec![0_u8; context.hash.read_size.max(1)];
        loop {
            match std::io::Read::read(&mut file, &mut buffer) {
                Ok(0) => break,
                Ok(count) => hasher.update(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error(&error)),
            }
        }
        if hasher.finish() != wanted.hashes {
            return Err(moved());
        }
    }
    Ok(Base {
        length: meta.len(),
        file,
        modified,
        block_size,
    })
}

fn run(
    channel: &Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
    poll: Duration,
) -> Result<WriteDone, ErrorBody> {
    let idle = context.config.idle_timeout.0;
    let request = WriteRequest::from_metadata(metadata)
        .map_err(|_| ErrorBody::new(ErrorCode::BadRequest, "the request cannot be read"))?;
    // A delta copies from the target, so a peer that may only write cannot use one to test guesses about it.
    if request.items {
        return super::bundle::write(channel, &request, context, poll);
    }
    let needs: &[Permission] = if matches!(request.mode, WriteMode::Delta { .. }) {
        &[Permission::FsWrite, Permission::FsRead]
    } else {
        &[Permission::FsWrite]
    };
    authorize(context, needs)?;
    check_request(&request)?;
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
    // One pass computes the union of what is answered and what is checked.
    let mut computed = answered.clone();
    for algorithm in request.expect.keys() {
        if !computed.contains(algorithm) {
            computed.push(algorithm.clone());
        }
    }
    let mut hasher = hasher_for(context, &computed)?;
    let _permit = context.slots.acquire().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Busy,
            "the host is running as many transfers as it allows",
        )
    })?;
    let roots = Roots::new(context.grant.fs_roots());
    let mut base = match &request.mode {
        WriteMode::Delta { block_size, base } => Some(open_base(
            &roots,
            context,
            &request.path,
            *block_size,
            base,
        )?),
        _ => None,
    };
    let (dir, relative) = roots.resolve(&request.path)?;
    let ttl = context.config.resume_ttl.0;
    let tag = || resume_tag(context.peer, &request.path);
    let no_resume = || {
        ErrorBody::new(
            ErrorCode::Unsupported,
            "this host keeps no write to continue ([filesystem] resume_ttl)",
        )
    };
    let mut ready = Ready::default();
    let mut resumed_at = 0_u64;
    let mut staged = match &request.mode {
        WriteMode::Resume { token } => {
            if ttl.is_zero() {
                return Err(no_resume());
            }
            let (mut staged, offset) = Staged::resume(&dir, &relative, &tag(), token, ttl)?;
            // The digests cover the whole file, so the bytes an earlier stream wrote go through them again.
            staged.read_back(&mut |bytes| hasher.update(bytes))?;
            ready.offset = Some(offset);
            resumed_at = offset;
            staged
        }
        _ if request.resume => {
            if ttl.is_zero() {
                return Err(no_resume());
            }
            let (staged, token) = Staged::open_resumable(
                &dir,
                &relative,
                request.parents,
                request.file_mode,
                &tag(),
                ttl,
            )?;
            ready.token = Some(token);
            staged
        }
        _ => Staged::open(
            &dir,
            &relative,
            &request.mode,
            request.parents,
            request.file_mode,
        )?,
    };

    let ended = || {
        ErrorBody::new(
            ErrorCode::Io,
            "the stream ended before the data was complete",
        )
    };
    if !send_waiting(
        channel.as_ref(),
        &encode(&WriteOutput::Ready(ready)),
        poll,
        idle,
    ) {
        staged.keep_for_resume();
        return Err(ended());
    }

    let mut written = resumed_at;
    // What arrived, which bounds what a delta may expand to.
    let mut received = 0_u64;
    let mut last_progress = Instant::now();
    loop {
        match channel.receive(Some(poll)) {
            Ok(Some(frame)) => match WriteInput::decode(&frame) {
                Ok(WriteInput::Copy(copy)) => {
                    received += frame.len() as u64;
                    let base = base.as_mut().ok_or_else(|| {
                        ErrorBody::new(ErrorCode::BadRequest, "a copy needs a delta write")
                    })?;
                    copy_len(base.length, base.block_size, copy.block, copy.count).ok_or_else(
                        || {
                            ErrorBody::new(
                                ErrorCode::BadRequest,
                                "a copy reaches past the end of the target",
                            )
                        },
                    )?;
                    let mut sink = Sink {
                        out: staged.file(),
                        hasher: &mut hasher,
                        written: &mut written,
                    };
                    apply(
                        &mut base.file,
                        &Op::Copy {
                            block: copy.block,
                            count: copy.count,
                        },
                        base.block_size,
                        &mut sink,
                    )
                    .map_err(|error| match error {
                        bohio_transfer::apply::ApplyError::Io(kind) => {
                            io_error(&std::io::Error::from(kind))
                        }
                        _ => ErrorBody::new(ErrorCode::BadRequest, error.to_string()),
                    })?;
                    last_progress = Instant::now();
                }
                Ok(WriteInput::Compressed(piece)) => {
                    let decoder = decoder.as_mut().ok_or_else(|| {
                        ErrorBody::new(
                            ErrorCode::BadRequest,
                            "compressed data needs a compression in the request",
                        )
                    })?;
                    let mut sink = Sink {
                        out: staged.file(),
                        hasher: &mut hasher,
                        written: &mut written,
                    };
                    received += piece.len() as u64;
                    decoder
                        .feed(&piece, &mut sink)
                        .map_err(|error| decode_failure(&error))?;
                    last_progress = Instant::now();
                }
                Ok(WriteInput::Data(bytes)) => {
                    // A raw frame ends the compression context.
                    if let Some(decoder) = decoder.as_mut() {
                        decoder.reset();
                    }
                    received += bytes.len() as u64;
                    staged
                        .file()
                        .write_all(&bytes)
                        .map_err(|error| io_error(&error))?;
                    hasher.update(&bytes);
                    written += bytes.len() as u64;
                    last_progress = Instant::now();
                }
                Ok(WriteInput::Flush) => {
                    if !matches!(request.mode, WriteMode::Append | WriteMode::At { .. }) {
                        return Err(ErrorBody::new(
                            ErrorCode::BadRequest,
                            "a flush is only for append and at",
                        ));
                    }
                    staged.flush(context.config.sync_writes)?;
                    let flushed = WriteOutput::Flushed(Flushed { written });
                    if !send_waiting(channel.as_ref(), &encode(&flushed), poll, idle) {
                        return Err(ended());
                    }
                    last_progress = Instant::now();
                }
                Ok(WriteInput::End) => break,
                // A frame this version does not know is skipped.
                Ok(_) | Err(_) => {}
            },
            Err(Failure::Timeout) => {
                if channel.is_closed() {
                    staged.keep_for_resume();
                    return Err(ended());
                }
                if last_progress.elapsed() >= idle {
                    staged.keep_for_resume();
                    return Err(ErrorBody::new(
                        ErrorCode::Io,
                        "the client sent nothing for too long",
                    ));
                }
            }
            Ok(None) | Err(_) => {
                staged.keep_for_resume();
                return Err(ended());
            }
        }
        let allowed = u128::from(context.delta.max_expansion);
        if let Some(base) = &base
            && allowed > 0
            && written > FREE_OUTPUT
            && u128::from(written) > allowed * u128::from(received + base.length)
        {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "the new file grows further than this host allows from what was sent",
            ));
        }
    }
    // Just before the rename: the target is still what the delta was made against.
    if let Some(base) = &base {
        let (dir, relative) = roots.resolve(&request.path)?;
        let (_, now) = ops::open_regular_in(&dir, &relative)?;
        if now.len() != base.length || now.modified().ok().map(ops::timestamp) != base.modified {
            return Err(ErrorBody::new(
                ErrorCode::Changed,
                "the target changed while the delta was written",
            ));
        }
    }

    let mut digests = hasher.finish();
    if let Some(name) = request
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
    staged.commit(context.config.sync_writes)?;
    let done = WriteDone {
        written,
        hashes: digests,
        index: None,
    };
    if !send_waiting(
        channel.as_ref(),
        &encode(&WriteOutput::Done(done.clone())),
        poll,
        idle,
    ) {
        // The data is in place; only the client's notice was lost.
        tracing::debug!("a write finished but its client was gone");
    }
    Ok(done)
}
