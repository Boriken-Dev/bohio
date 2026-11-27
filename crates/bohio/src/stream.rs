//! Sending on a stream without growing a buffer: wait while the stream has no room.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use coqui_transport::{Channel, Failure};

/// Sends one message, waiting while the stream has no room. `false` when the stream failed or
/// closed, or when `idle` passed without the stream taking the message; the message was then not
/// sent.
pub(crate) fn send_waiting(
    channel: &dyn Channel,
    message: &[u8],
    poll: Duration,
    idle: Duration,
) -> bool {
    let started = Instant::now();
    let over = || channel.is_closed() || started.elapsed() >= idle;
    send_until(channel, message, poll, &over).is_ok()
}

/// Why a message was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unsent {
    /// `stop` said to give up while the stream had no room.
    Stopped,
    /// The stream failed or closed.
    Failed,
}

/// Sends one message, waiting while the stream has no room, until `stop` says to give up. The message was not sent
/// when this is an error.
pub(crate) fn send_until(
    channel: &dyn Channel,
    message: &[u8],
    poll: Duration,
    stop: &dyn Fn() -> bool,
) -> Result<(), Unsent> {
    loop {
        match channel.send_timeout(message, Some(poll)) {
            Ok(()) => return Ok(()),
            Err(Failure::Timeout | Failure::NoProgress) => {
                if stop() {
                    return Err(Unsent::Stopped);
                }
            }
            Err(_) => return Err(Unsent::Failed),
        }
    }
}

/// The most data bytes in one frame: the configured size, less the tag byte a stream message
/// needs, within what the stream carries.
pub(crate) fn chunk_size(channel: &dyn Channel, configured: usize) -> usize {
    channel
        .capabilities()
        .max_send
        .map_or(configured, |max| configured.min(max.saturating_sub(1)))
        .max(1)
}

/// The largest message the stream carries; unbounded when it reports none.
pub(crate) fn message_limit(channel: &dyn Channel) -> usize {
    channel.capabilities().max_send.unwrap_or(usize::MAX)
}
