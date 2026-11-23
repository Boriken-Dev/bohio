//! The host's half of a delta read: take the client's signature, then send only what its old copy lacks.

use std::io::Read;
use std::time::{Duration, Instant};

use bohio_proto::compress::Compression;
use bohio_proto::fs::{
    DeltaCopy, ReadDelta, ReadDone, ReadHeader, ReadInput, ReadOutput, WEAK_RSYNC,
};
use bohio_proto::{ErrorBody, ErrorCode};
use bohio_transfer::compress::{Encoder, Piece};
use bohio_transfer::delta::{Matcher, Op};
use bohio_transfer::hash::Hasher;
use bohio_transfer::signature::{Signature, SignatureSpec};
use coqui_transport::{Channel, Failure};

use super::compress::Settings;
use super::hash::allowed;
use super::{Context, io_error};
use crate::stream::send_waiting;

/// What a delta read needs besides the file: the open's options, the stream and the bounds.
pub(super) struct Pull<'a> {
    pub channel: &'a dyn Channel,
    pub context: &'a Context<'a>,
    pub delta: &'a ReadDelta,
    pub settings: &'a Settings<'a>,
    pub compression: Option<&'a Compression>,
    pub size: u64,
    pub chunk: usize,
    pub poll: Duration,
    pub idle: Duration,
}

/// The signature the client sends, checked against the bounds, before any byte of the file is read.
///
/// # Errors
///
/// `unsupported` for a weak checksum or strong hash this host lacks, `bad_request` for a block size or length
/// that cannot be used, a batch out of order or a signature that ends short, `busy` for a signature past
/// `[delta] max_signature_bytes`, and `io` for a stream that ended or stayed idle.
fn receive_signature(pull: &Pull<'_>) -> Result<Signature, ErrorBody> {
    let delta = pull.delta;
    if delta.weak != WEAK_RSYNC {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "this host knows only the rsync weak checksum",
        ));
    }
    allowed(pull.context, std::slice::from_ref(&delta.strong))?;
    let max_block = pull.context.delta.max_block;
    let block = u32::try_from(delta.block_size)
        .ok()
        .filter(|block| (1..=max_block).contains(block))
        .ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::BadRequest,
                "the block size is from 1 to this host's [delta] max_block",
            )
        })?;
    let strong_len = usize::try_from(delta.strong_len).unwrap_or(usize::MAX);
    let spec = SignatureSpec::new(block, delta.strong.clone(), strong_len)
        .map_err(|error| ErrorBody::new(ErrorCode::BadRequest, error.to_string()))?;
    let mut signature = Signature::new(spec, delta.base_size)
        .map_err(|error| ErrorBody::new(ErrorCode::BadRequest, error.to_string()))?;
    let needed = signature
        .blocks_needed()
        .saturating_mul(4 + delta.strong_len);
    let too_big = || {
        ErrorBody::new(
            ErrorCode::Busy,
            "the signature is larger than this host takes (max_signature_bytes)",
        )
    };
    if needed > pull.context.delta.max_signature_bytes {
        return Err(too_big());
    }
    let mut last_progress = Instant::now();
    while !end_seen(pull, &mut last_progress, &mut signature)? {}
    Ok(signature)
}

/// Reads frames until the client's end of signature; `true` when it arrived. Batches go into `signature`.
fn end_seen(
    pull: &Pull<'_>,
    last_progress: &mut Instant,
    signature: &mut Signature,
) -> Result<bool, ErrorBody> {
    let ended = || {
        ErrorBody::new(
            ErrorCode::Io,
            "the stream ended before the signature was complete",
        )
    };
    match pull.channel.receive(Some(pull.poll)) {
        Ok(Some(frame)) => {
            match ReadInput::decode(&frame) {
                Ok(ReadInput::Signature(batch)) => {
                    if batch.first != signature.blocks() {
                        return Err(ErrorBody::new(
                            ErrorCode::BadRequest,
                            "the signature's batches are out of order",
                        ));
                    }
                    signature
                        .push_batch(&batch.weak, &batch.strong)
                        .map_err(|error| {
                            ErrorBody::new(ErrorCode::BadRequest, error.to_string())
                        })?;
                    *last_progress = Instant::now();
                    Ok(false)
                }
                Ok(ReadInput::SignatureEnd) => {
                    if signature.is_complete() {
                        Ok(true)
                    } else {
                        Err(ErrorBody::new(
                            ErrorCode::BadRequest,
                            "the signature ended before every block of the old copy",
                        ))
                    }
                }
                // A frame this version does not know is skipped.
                Ok(_) | Err(_) => Ok(false),
            }
        }
        Err(Failure::Timeout) => {
            if pull.channel.is_closed() {
                return Err(ended());
            }
            if last_progress.elapsed() >= pull.idle {
                return Err(ErrorBody::new(
                    ErrorCode::Io,
                    "the client sent nothing for too long",
                ));
            }
            Ok(false)
        }
        Ok(None) | Err(_) => Err(ended()),
    }
}

/// Serves a delta read of `file`: the header, then `copy` and data frames, then `done`. Returns the file's size.
///
/// # Errors
///
/// As [`receive_signature`], and `io` for a file that shrank or a stream that ended.
pub(super) fn pull(
    pull: &Pull<'_>,
    file: &mut impl Read,
    mut hasher: Hasher,
    skip_compression: bool,
) -> Result<u64, ErrorBody> {
    let signature = receive_signature(pull)?;
    let mut encoder = None;
    if let Some(compression) = pull.compression
        && pull.size > 0
        && !skip_compression
    {
        encoder = Some(
            Encoder::new(compression, pull.settings.caps())
                .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))?,
        );
    }
    let used = encoder.as_ref().map(|encoder| encoder.used().clone());
    let give_up = || ErrorBody::new(ErrorCode::Io, "the stream ended before the file was sent");
    let send = |output: &ReadOutput| {
        send_waiting(
            pull.channel,
            &output.encode().unwrap_or_else(|_| vec![0x05]),
            pull.poll,
            pull.idle,
        )
    };
    let header = ReadHeader {
        compression: used,
        delta: true,
        ..ReadHeader::raw(pull.size, pull.size)
    };
    if !send(&ReadOutput::Header(header)) {
        return Err(give_up());
    }
    // A literal run is one frame: it fits what the stream carries.
    let mut matcher = Matcher::new(signature, pull.chunk)
        .map_err(|error| ErrorBody::new(ErrorCode::BadRequest, error.to_string()))?;
    let mut buffer = vec![0_u8; pull.chunk];
    let mut remaining = pull.size;
    let mut ship = |ops: Vec<Op>| -> Result<(), ErrorBody> {
        for op in ops {
            let frame = match op {
                Op::Copy { block, count } => ReadOutput::Copy(DeltaCopy { block, count }),
                Op::Literal(bytes) => match encoder.as_mut() {
                    Some(encoder) => match encoder
                        .frame(&bytes)
                        .map_err(|error| ErrorBody::new(ErrorCode::Internal, error.to_string()))?
                    {
                        Piece::Compressed(piece) => ReadOutput::Compressed(piece),
                        Piece::Raw => ReadOutput::Data(bytes),
                    },
                    None => ReadOutput::Data(bytes),
                },
            };
            if !send(&frame) {
                return Err(give_up());
            }
        }
        Ok(())
    };
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
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
        ship(matcher.feed(&buffer[..count]))?;
    }
    ship(matcher.finish())?;
    let hashes = hasher.finish();
    if !send(&ReadOutput::Done(ReadDone {
        hashes,
        index: None,
    })) {
        return Err(give_up());
    }
    Ok(pull.size)
}
