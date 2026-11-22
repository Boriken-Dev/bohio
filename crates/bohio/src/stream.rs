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
    loop {
        match channel.send_timeout(message, Some(poll)) {
            Ok(()) => return true,
            Err(Failure::Timeout | Failure::NoProgress) => {
                if channel.is_closed() || started.elapsed() >= idle {
                    return false;
                }
            }
            Err(_) => return false,
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
