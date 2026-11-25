//! Digests of files on a host: `bohio-hash/1`.

use std::thread;
use std::time::Duration;

use bohio_proto::hash::{
    Algorithm, BlockOptions, Digests, HashInput, HashItem, HashOutput, HashRequest, HashResult,
};
use bohio_proto::{ErrorBody, protocol};
use bohio_transfer::signature::{Signature, SignatureSpec, WEAK_RSYNC};
use coqui_transport::Channel;

use crate::{Error, Open};

/// The digests of one file, or of a range of it.
///
/// # Errors
///
/// [`Error::Remote`] when the host refused the stream (`unsupported` for an algorithm it does not
/// offer, `busy` at its hash-stream limit, `denied`) or failed this file (`not_found`,
/// `outside_root`, `io`, ...); [`Error::Closed`] and [`Error::Transport`] otherwise.
pub fn hash_file(link: &dyn Open, path: &str, algorithms: &[Algorithm]) -> Result<Digests, Error> {
    hash_range(link, HashItem::whole(path), algorithms)
}

/// The digests of the bytes `item` names (a range of a file); see [`hash_file`] for the errors.
///
/// # Errors
///
/// As [`hash_file`].
pub fn hash_range(
    link: &dyn Open,
    item: HashItem,
    algorithms: &[Algorithm],
) -> Result<Digests, Error> {
    let mut answers = hash_items(link, &HashRequest::new(algorithms.iter().cloned()), &[item])?;
    match answers.pop() {
        Some(Ok(result)) => Ok(result.hashes),
        Some(Err(body)) => Err(Error::Remote(body)),
        None => Err(Error::Closed),
    }
}

/// The digests of many files on one stream, in the order of `items`: each is its own result, so a
/// file that is missing does not fail the others.
///
/// # Errors
///
/// As [`hash_file`], for the stream as a whole.
pub fn hash_many(
    link: &dyn Open,
    items: &[HashItem],
    algorithms: &[Algorithm],
) -> Result<Vec<Result<Digests, ErrorBody>>, Error> {
    let request = HashRequest::new(algorithms.iter().cloned());
    Ok(hash_items(link, &request, items)?
        .into_iter()
        .map(|answer| answer.map(|result| result.hashes))
        .collect())
}

/// As [`hash_many`], keeping each item's size, modification time and `unchanged` flag, and with the whole
/// request (`algorithms` and the rest of the options) given.
///
/// Items are sent and results read on two threads, so a long batch cannot stop on a full stream: the host
/// answers each item in order while the next are still being sent.
///
/// # Errors
///
/// As [`hash_file`], for the stream as a whole; [`Error::Closed`] when the stream ended before every item
/// was answered.
pub fn hash_items(
    link: &dyn Open,
    request: &HashRequest,
    items: &[HashItem],
) -> Result<Vec<Result<HashResult, ErrorBody>>, Error> {
    let channel = link.open(protocol::HASH, &request.to_metadata()?)?;
    let channel: &dyn Channel = channel.as_ref();
    thread::scope(|scope| {
        let sender = scope.spawn(move || send_items(channel, items));
        let received = receive_answers(channel, items.len());
        // Closing ends a send still waiting for room, which a host that gave up no longer makes.
        channel.close();
        // A failed send is told by the host's own error frame when there is one, and by the stream's end otherwise.
        let _ = sender.join();
        received
    })
}

/// The block signature of a file as the host made it, with what the host said about the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocks {
    /// The host's answer for the item: the file's size and modification time when it was opened, and the digests.
    pub result: HashResult,
    /// The weak and strong hash of every block of the hashed range.
    pub signature: Signature,
    /// How many bytes of frames arrived: what the signature cost on the wire.
    pub wire_bytes: u64,
}

/// The block signature of the file `item` names, made on the host: its blocks' weak and strong hashes
/// (the strong hash is `algorithms[0]` cut to `options.strong_len`), for matching a local file against it.
/// `options.size` is the block size, and the host chooses from the file's size when it is absent.
///
/// # Errors
///
/// As [`hash_file`]; [`Error::Protocol`] for blocks that do not add up to the file.
pub fn hash_blocks(
    link: &dyn Open,
    item: HashItem,
    algorithms: &[Algorithm],
    options: &BlockOptions,
) -> Result<Blocks, Error> {
    let strong = algorithms
        .first()
        .cloned()
        .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
    let request = HashRequest {
        algorithms: algorithms.to_vec(),
        source: None,
        blocks: Some(BlockOptions {
            weak: Some(WEAK_RSYNC.to_owned()),
            ..options.clone()
        }),
    };
    let channel = link.open(protocol::HASH, &request.to_metadata()?)?;
    let channel: &dyn Channel = channel.as_ref();
    let bad = || Error::Protocol(bohio_proto::Error::Decode);
    let outcome = (|| {
        channel.send_timeout(&HashInput::Item(item).encode()?, None)?;
        channel.send_timeout(&HashInput::End.encode()?, None)?;
        let mut weak: Vec<u32> = Vec::new();
        let mut hashes: Vec<u8> = Vec::new();
        let mut wire = 0_u64;
        let mut result = None;
        loop {
            let Some(frame) = channel.receive(None::<Duration>)? else {
                return Err(Error::Closed);
            };
            wire += frame.len() as u64;
            match HashOutput::decode(&frame) {
                Ok(HashOutput::Blocks(batch)) => {
                    if batch.first != weak.len() as u64 {
                        return Err(bad());
                    }
                    weak.extend_from_slice(&batch.weak);
                    hashes.extend_from_slice(&batch.strong);
                }
                Ok(HashOutput::Result(found)) => result = Some(found),
                Ok(HashOutput::ItemError(failure)) => return Err(Error::Remote(failure.body())),
                Ok(HashOutput::Done(_)) => break,
                Ok(HashOutput::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let result = result.ok_or_else(bad)?;
        let (size, strong_len) = result.block_size.zip(result.strong_len).ok_or_else(bad)?;
        let block_size = u32::try_from(size).map_err(|_| bad())?;
        let spec = SignatureSpec::new(
            block_size,
            strong,
            usize::try_from(strong_len).map_err(|_| bad())?,
        )
        .map_err(|_| bad())?;
        let mut signature = Signature::new(spec, result.length).map_err(|_| bad())?;
        signature.push_batch(&weak, &hashes).map_err(|_| bad())?;
        if !signature.is_complete() {
            return Err(bad());
        }
        Ok(Blocks {
            result,
            signature,
            wire_bytes: wire,
        })
    })();
    channel.close();
    outcome
}

fn send_items(channel: &dyn Channel, items: &[HashItem]) -> Result<(), Error> {
    for item in items {
        let frame = HashInput::Item(item.clone()).encode()?;
        channel.send_timeout(&frame, None)?;
    }
    channel.send_timeout(&HashInput::End.encode()?, None)?;
    Ok(())
}

fn receive_answers(
    channel: &dyn Channel,
    count: usize,
) -> Result<Vec<Result<HashResult, ErrorBody>>, Error> {
    let mut answers: Vec<Option<Result<HashResult, ErrorBody>>> = vec![None; count];
    let place = |answers: &mut Vec<Option<_>>, index: u64, answer| {
        let slot = usize::try_from(index)
            .ok()
            .and_then(|at| answers.get_mut(at))
            .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
        *slot = Some(answer);
        Ok::<(), Error>(())
    };
    loop {
        let Some(frame) = channel.receive(None::<Duration>)? else {
            return Err(Error::Closed);
        };
        match HashOutput::decode(&frame) {
            Ok(HashOutput::Result(result)) => place(&mut answers, result.index, Ok(result))?,
            Ok(HashOutput::ItemError(failure)) => {
                place(&mut answers, failure.index, Err(failure.body()))?;
            }
            Ok(HashOutput::Done(_)) => {
                return answers
                    .into_iter()
                    .collect::<Option<Vec<_>>>()
                    .ok_or(Error::Closed);
            }
            Ok(HashOutput::Error(body)) => return Err(Error::Remote(body)),
            Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
}
