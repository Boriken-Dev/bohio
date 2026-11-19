//! The bounds of a link, the same for a host and a client.

use std::io;
use std::net::TcpStream;
use std::time::Duration;

use coqui_link::auth::MAX_PROOF_MESSAGE;
use coqui_link::mux::Limits;
use coqui_transport::frame::LengthPrefix;
use coqui_transport::{StreamChannel, StreamChannelBuilder};

/// What a link holds and how long it waits, with a documented default for each.
///
/// What one stream can buffer is bounded by `window` (what the peer may send ahead of what is read) plus
/// `queue_bytes` in each direction of its relay; what one link can buffer besides is `read_ahead`
/// messages of `max_frame` bytes read from the socket and `queued_bytes` waiting to be written. Nothing
/// grows with the size of a file or an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Settings {
    /// The most streams open at once, in both directions together. At least 1. Default 64.
    pub max_streams: usize,
    /// How long a peer has to prove itself. Default 10 seconds.
    pub proof_timeout: Duration,
    /// Bytes a peer may send on one stream ahead of what has been read from it: the stream's credit
    /// window. Default 1 MiB.
    pub window: u32,
    /// The most bytes of one message in one frame, so a long message cannot hold other streams up. Default
    /// 64 KiB; at least a `chunk_size` plus its tag.
    pub max_frame: usize,
    /// The largest message on a stream. Default 1 MiB.
    pub max_message: usize,
    /// Bytes a stream's relay holds in each direction; a message larger than this passes alone. Default
    /// 256 KiB.
    pub queue_bytes: usize,
    /// Messages the socket's reader holds ahead of the link once the peer is proven (one before). Default 8.
    pub read_ahead: usize,
    /// Bytes the socket's writer may have queued. Default 1 MiB.
    pub queued_bytes: usize,
    /// How long a frame the socket takes nothing of ends the link, and how long a closed stream's
    /// queue is given to drain. Default 10 seconds.
    pub send_limit: Duration,
    /// How long a send waits for credit before it reports no progress, and is tried again. Default
    /// 250 ms.
    pub send_wait: Duration,
    /// How often a stream's threads look at whether it was closed. Default 50 ms.
    pub poll_interval: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_streams: 64,
            proof_timeout: Duration::from_secs(10),
            window: 1024 * 1024,
            max_frame: 64 * 1024,
            max_message: 1024 * 1024,
            queue_bytes: 256 * 1024,
            read_ahead: 8,
            queued_bytes: 1024 * 1024,
            send_limit: Duration::from_secs(10),
            send_wait: Duration::from_millis(250),
            poll_interval: Duration::from_millis(50),
        }
    }
}

impl Settings {
    /// The mux limits these settings make.
    pub fn limits(&self) -> Limits {
        let max_streams = self.max_streams;
        let mut limits = Limits::default();
        limits.max_streams = max_streams.max(1);
        limits.max_per_protocol = max_streams.max(1);
        limits.window = self.window.max(1);
        limits.max_frame = self.max_frame.max(1);
        limits.max_message = self.max_message.max(1);
        limits.send_wait = self.send_wait;
        limits
    }

    /// The largest message the socket carries once the peer is proven, for a proof by `method`.
    pub fn record_length(&self, method: coqui_link::auth::Method) -> usize {
        method.record_length(self.max_frame.max(1).saturating_add(5))
    }
}

/// A channel over `stream` that holds an unproven peer to one message of at most the proof's size,
/// until [`Settings::read_ahead`] is raised after the proof.
///
/// # Errors
///
/// The error of starting the channel's threads or setting the socket.
pub fn socket(stream: TcpStream, settings: &Settings) -> io::Result<StreamChannel> {
    stream.set_nodelay(true)?;
    StreamChannelBuilder::new()
        .codec(LengthPrefix::u32_be())
        .stream(stream)
        .max_message(MAX_PROOF_MESSAGE)
        .pending(1)
        .queued_bytes(settings.queued_bytes.max(1))
        .send_limit(settings.send_limit)
        .build()
        .map_err(|error| io::Error::other(error.to_string()))
}
