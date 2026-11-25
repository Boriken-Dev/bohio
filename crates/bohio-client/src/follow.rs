//! A file of the host followed as it grows (`tail -f`): `bohio-read/1` with `follow`.

use std::sync::Arc;
use std::time::Duration;

use bohio_proto::fs::{FollowBy, ReadFollow, ReadOutput, ReadRequest, Truncated};
use bohio_proto::protocol;
use cabuya::mux::LinkStream;

use crate::{Error, Open};

/// What a follow reads, and how the host watches the file.
#[derive(Debug, Clone, Default)]
pub struct FollowOptions {
    /// Start this many bytes before the end of the file (`Some(0)`: only what is appended comes); a file shorter than that
    /// is sent from its start. `None` starts at `offset`.
    pub tail: Option<u64>,
    /// The first byte to send when `tail` is `None`. Default `0`.
    pub offset: u64,
    /// Look the path up again at every look, so a file that takes its name (a log rotation) is followed from its start.
    /// Otherwise the file that was opened is followed, even when it is renamed or removed.
    pub by_name: bool,
    /// How often the host looks for more bytes, at least 1 ms; `None` uses the host's `[filesystem] follow_interval`.
    pub interval: Option<Duration>,
}

/// One thing a follow delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowEvent {
    /// Bytes of the file, in order.
    Data(Vec<u8>),
    /// The file got shorter or was replaced; what comes next continues from the host's new position.
    Truncated(Truncated),
}

/// A followed file, as it arrives. Dropping it closes the stream, which ends the follow on the host.
pub struct Follow {
    channel: Arc<dyn LinkStream>,
    size: u64,
    length: u64,
    ended: bool,
}

impl core::fmt::Debug for Follow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Follow")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// Starts following `path` on the host: the bytes from where [`FollowOptions`] says the read starts, then what is
/// appended, until the [`Follow`] is dropped or the link ends. Waits at most `timeout` for the host to accept it.
///
/// # Errors
///
/// [`Error::Remote`] when the host refused: `denied` without `fs.read`, `not_found` or `outside_root` for the path,
/// `bad_request` for an interval of zero or an offset past the end, `busy` at its `max_follows`. [`Error::Timeout`] or
/// [`Error::Closed`] when no answer came.
pub fn follow(
    link: &dyn Open,
    path: &str,
    options: &FollowOptions,
    timeout: Option<Duration>,
) -> Result<Follow, Error> {
    let mut request = ReadRequest::whole(path);
    request.offset = options.offset;
    request.follow = Some(ReadFollow {
        by: options.by_name.then_some(FollowBy::Name),
        interval_ms: options
            .interval
            .map(|wait| u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)),
        tail: options.tail,
    });
    let channel: Arc<dyn LinkStream> =
        Arc::from(link.open(protocol::READ, &request.to_metadata()?)?);
    loop {
        let Some(frame) = channel.receive(timeout)? else {
            return Err(Error::Closed);
        };
        match ReadOutput::decode(&frame) {
            Ok(ReadOutput::Header(header)) => {
                return Ok(Follow {
                    channel,
                    size: header.size,
                    length: header.length,
                    ended: false,
                });
            }
            Ok(ReadOutput::Error(body)) => return Err(Error::Remote(body)),
            Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

impl Follow {
    /// The file's size when the follow started.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// How many bytes the host sends before it has caught up with the end of the file as it was when the follow started.
    pub const fn initial_length(&self) -> u64 {
        self.length
    }

    /// The next event, waiting at most `wait`: `Ok(None)` when nothing came in that time (the follow goes on).
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host ended the follow with an error (the file could not be read, the grant was taken
    /// away), [`Error::Closed`] when the link ended.
    pub fn next_within(&mut self, wait: Option<Duration>) -> Result<Option<FollowEvent>, Error> {
        loop {
            if self.ended {
                return Err(Error::Closed);
            }
            let frame = match self.channel.receive(wait) {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    self.ended = true;
                    return Err(Error::Closed);
                }
                Err(coqui_transport::Failure::Timeout) => return Ok(None),
                Err(failure) => {
                    self.ended = true;
                    return Err(failure.into());
                }
            };
            match ReadOutput::decode(&frame) {
                Ok(ReadOutput::Data(bytes)) => return Ok(Some(FollowEvent::Data(bytes))),
                Ok(ReadOutput::Truncated(truncated)) => {
                    return Ok(Some(FollowEvent::Truncated(truncated)));
                }
                Ok(ReadOutput::Error(body)) => {
                    self.ended = true;
                    return Err(Error::Remote(body));
                }
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Reads for at most `window` and returns what arrived in it, as one list, so a caller that wants "what was appended
    /// in the next ten seconds" does not loop. Ends early when the host ends the follow.
    ///
    /// # Errors
    ///
    /// As [`Follow::next_within`], once nothing arrived before the error.
    pub fn collect_for(&mut self, window: Duration) -> Result<Vec<FollowEvent>, Error> {
        let until = std::time::Instant::now() + window;
        let mut events = Vec::new();
        loop {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Ok(events);
            }
            match self.next_within(Some(left)) {
                Ok(Some(event)) => events.push(event),
                Ok(None) => return Ok(events),
                Err(error) if events.is_empty() => return Err(error),
                Err(_) => return Ok(events),
            }
        }
    }
}

impl Iterator for Follow {
    type Item = Result<FollowEvent, Error>;

    /// Waits without limit; ends when the link or the follow ends.
    fn next(&mut self) -> Option<Self::Item> {
        match self.next_within(None) {
            Ok(Some(event)) => Some(Ok(event)),
            Ok(None) | Err(Error::Closed) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        self.channel.close();
    }
}
