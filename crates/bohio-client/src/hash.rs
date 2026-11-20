//! Digests of files on a host: `bohio-hash/1`.

use std::thread;
use std::time::Duration;

use bohio_proto::hash::{
    Algorithm, Digests, HashInput, HashItem, HashOutput, HashRequest, HashResult,
};
use bohio_proto::{ErrorBody, protocol};
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
