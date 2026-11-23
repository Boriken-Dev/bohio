//! `bohio-power/1`: the power of the system a node is for.

use std::time::Duration;

use bohio_proto::power::{Action, Answer, Request, State};
use bohio_proto::protocol;

use crate::{Error, Open};

/// Asks the node's power source (`source`, or its default) to `action`, and waits up to `timeout` (`None`: without limit) for
/// the answer. A status changes nothing; every other action changes the system's power.
///
/// # Errors
///
/// [`Error::Remote`] when the host refused or the source failed (`denied`, `unsupported`, `timeout`...), [`Error::Timeout`],
/// [`Error::Closed`], or whatever `link` reports.
pub fn act(
    link: &dyn Open,
    source: Option<&str>,
    action: Action,
    timeout: Option<Duration>,
) -> Result<State, Error> {
    let mut request = Request::new(action);
    request.source = source.map(str::to_owned);
    let channel = link.open(protocol::POWER, &request.to_metadata()?)?;
    let answer = channel.receive(timeout);
    channel.close();
    match answer {
        Ok(Some(frame)) => match Answer::decode(&frame)? {
            Answer::State(state) => Ok(state),
            Answer::Error(body) => Err(Error::Remote(body)),
        },
        Ok(None) => Err(Error::Closed),
        Err(failure) => Err(Error::from(failure)),
    }
}

/// The state of the system's power: `act` with [`Action::Status`].
///
/// # Errors
///
/// As [`act`].
pub fn status(
    link: &dyn Open,
    source: Option<&str>,
    timeout: Option<Duration>,
) -> Result<State, Error> {
    act(link, source, Action::Status, timeout)
}
