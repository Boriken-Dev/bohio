//! `bohio-media/1`: virtual media of the system a node is for.

use std::time::Duration;

use bohio_proto::media::{Answer, Image, Op, Request, Slot};
use bohio_proto::protocol;

use crate::{Error, Open};

/// Does `request` and waits up to `timeout` (`None`: without limit) for the slots after it.
///
/// # Errors
///
/// [`Error::Remote`] when the host refused or the source failed (`denied`, `unsupported`, `outside_root`, `timeout`...),
/// [`Error::Timeout`], [`Error::Closed`], or whatever `link` reports.
pub fn call(
    link: &dyn Open,
    request: &Request,
    timeout: Option<Duration>,
) -> Result<Vec<Slot>, Error> {
    let channel = link.open(protocol::MEDIA, &request.to_metadata()?)?;
    let answer = channel.receive(timeout);
    channel.close();
    match answer {
        Ok(Some(frame)) => match Answer::decode(&frame)? {
            Answer::Slots(slots) => Ok(slots.slots),
            Answer::Error(body) => Err(Error::Remote(body)),
            _ => Err(Error::Protocol(bohio_proto::Error::Decode)),
        },
        Ok(None) => Err(Error::Closed),
        Err(failure) => Err(Error::from(failure)),
    }
}

/// The slots and what each holds.
///
/// # Errors
///
/// As [`call`].
pub fn status(
    link: &dyn Open,
    source: Option<&str>,
    timeout: Option<Duration>,
) -> Result<Vec<Slot>, Error> {
    let mut request = Request::new(Op::Status);
    request.source = source.map(str::to_owned);
    call(link, &request, timeout)
}

/// Inserts `image` in `slot`.
///
/// # Errors
///
/// As [`call`].
pub fn insert(
    link: &dyn Open,
    source: Option<&str>,
    slot: &str,
    image: Image,
    timeout: Option<Duration>,
) -> Result<Vec<Slot>, Error> {
    let mut request = Request::new(Op::Insert);
    request.source = source.map(str::to_owned);
    request.slot = Some(slot.to_owned());
    request.image = Some(image);
    call(link, &request, timeout)
}

/// Ejects what `slot` holds.
///
/// # Errors
///
/// As [`call`].
pub fn eject(
    link: &dyn Open,
    source: Option<&str>,
    slot: &str,
    timeout: Option<Duration>,
) -> Result<Vec<Slot>, Error> {
    let mut request = Request::new(Op::Eject);
    request.source = source.map(str::to_owned);
    request.slot = Some(slot.to_owned());
    call(link, &request, timeout)
}
