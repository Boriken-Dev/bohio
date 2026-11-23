//! A host's desktop: `bohio-gui/1`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use bohio_proto::gui::{
    ClipboardGet, ClipboardSet, DisplayId, GuiInput, GuiOutput, GuiRequest, ImageFormat,
    ImageHeader, Input, InputEvent, Point, Ready, Rect, Settle, SettleOptions, Snapshot as Wire,
    State, Want,
};
use bohio_proto::protocol;
use coqui_transport::Channel;

use crate::{Error, Open};

/// What a snapshot asks for: every field is optional and the host fills what is absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotOptions {
    /// The display; the primary one when absent.
    pub display: Option<DisplayId>,
    /// The part of the display; all of it when absent.
    pub region: Option<Rect>,
    /// The format; the host first allowed one when absent.
    pub format: Option<ImageFormat>,
    /// JPEG quality, 1 to 100.
    pub quality: Option<u8>,
    /// Scale down to fit this width.
    pub max_width: Option<u32>,
    /// Scale down to fit this height.
    pub max_height: Option<u32>,
    /// Report where the pointer is.
    pub cursor: bool,
    /// Wait for the screen to settle first.
    pub settle: Option<SettleOptions>,
}

/// An image of the screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    /// What the host said about it: the display, size, region, scale, pointer and settle.
    pub header: ImageHeader,
    /// The encoded image.
    pub bytes: Vec<u8>,
}

/// How a settle ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleResult {
    /// The screen stayed unchanged for the quiet time.
    pub settled: bool,
    /// How long the host waited, in milliseconds.
    pub waited_ms: u64,
}

/// An open `bohio-gui/1` stream. Requests are answered in order; every method takes `&self` and calls from several
/// threads take turns.
pub struct Gui {
    channel: Box<dyn Channel>,
    ready: Ready,
    state: Mutex<State>,
    next: Mutex<u64>,
    turn: Mutex<()>,
}

impl core::fmt::Debug for Gui {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Gui").finish_non_exhaustive()
    }
}

/// How long [`Gui::open`] waits for the host to say it is ready: a host may be waiting for the person at the desk.
pub const OPEN_WAIT: Duration = Duration::from_secs(120);

impl Gui {
    /// Opens a stream on `link` and waits for the host to accept it (up to [`OPEN_WAIT`]).
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] when the host refused (`denied`, `unsupported`, `busy`, `unavailable`...), [`Error::Closed`] and
    /// [`Error::Transport`] otherwise (a grant the host does not give is a `Permission` failure of the transport).
    pub fn open(link: &dyn Open, request: &GuiRequest) -> Result<Self, Error> {
        Self::open_within(link, request, Some(OPEN_WAIT))
    }

    /// As [`Gui::open`], waiting up to `wait` for each frame of the host's answer (`None` without limit): a host that
    /// asks its owner to approve needs as long as the owner takes.
    ///
    /// # Errors
    ///
    /// As [`Gui::open`]; [`Error::Timeout`] when the host says nothing within `wait`.
    pub fn open_within(
        link: &dyn Open,
        request: &GuiRequest,
        wait: Option<Duration>,
    ) -> Result<Self, Error> {
        let channel = link.open(protocol::GUI, &request.to_metadata()?)?;
        loop {
            let Some(frame) = channel.receive(wait)? else {
                return Err(Error::Closed);
            };
            match GuiOutput::decode(&frame) {
                Ok(GuiOutput::Ready(ready)) => {
                    return Ok(Self {
                        state: Mutex::new(ready.state.clone()),
                        ready,
                        channel,
                        next: Mutex::new(0),
                        turn: Mutex::new(()),
                    });
                }
                Ok(GuiOutput::Error(body)) => return Err(Error::Remote(body)),
                Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// What the stream was opened for and the host granted.
    pub fn granted(&self) -> &[Want] {
        &self.ready.granted
    }

    /// The desktop session served.
    pub fn session(&self) -> &str {
        &self.ready.session
    }

    /// The desktop as the host last said it (at the open, and in each `state` frame read since).
    pub fn state(&self) -> State {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn seq(&self) -> u64 {
        let mut next = self
            .next
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *next += 1;
        *next
    }

    /// Sends `frame` and passes each answer frame to `take` until it returns a value; `state` frames are kept, and frames of
    /// another request are skipped.
    fn ask<T>(
        &self,
        seq: u64,
        frame: &GuiInput,
        timeout: Option<Duration>,
        mut take: impl FnMut(GuiOutput) -> Option<Result<T, Error>>,
    ) -> Result<T, Error> {
        let _turn = self
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = timeout.map(|limit| Instant::now() + limit);
        self.channel.send_timeout(&frame.encode()?, timeout)?;
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(bytes) = self.channel.receive(left)? else {
                return Err(Error::Closed);
            };
            match GuiOutput::decode(&bytes) {
                Ok(GuiOutput::State(state)) => {
                    *self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
                }
                Ok(GuiOutput::RequestError(failure)) if failure.seq == seq || failure.seq == 0 => {
                    return Err(Error::Remote(failure.body()));
                }
                Ok(GuiOutput::Error(body)) => return Err(Error::Remote(body)),
                Ok(output) => {
                    if let Some(done) = take(output) {
                        return done;
                    }
                }
                Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// An image of a display or a region of one.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] with the host's reason (`denied`, `busy` past `snapshots_per_minute`, `bad_request`,
    /// `unsupported` for a format it does not offer...), [`Error::Timeout`], [`Error::Closed`].
    pub fn snapshot(
        &self,
        options: &SnapshotOptions,
        timeout: Option<Duration>,
    ) -> Result<Image, Error> {
        let seq = self.seq();
        let wire = Wire {
            seq,
            display: options.display.clone(),
            region: options.region,
            format: options.format,
            quality: options.quality,
            max_width: options.max_width,
            max_height: options.max_height,
            cursor: options.cursor,
            settle: options.settle,
        };
        let mut header: Option<ImageHeader> = None;
        let mut bytes = Vec::new();
        self.ask(
            seq,
            &GuiInput::Snapshot(wire),
            timeout,
            |output| match output {
                GuiOutput::Image(head) if head.seq == seq && header.is_none() => {
                    header = Some(head);
                    None
                }
                GuiOutput::Data(piece) if header.is_some() => {
                    bytes.extend_from_slice(&piece);
                    let head = header.as_ref()?;
                    if bytes.len() as u64 > head.length {
                        return Some(Err(Error::Protocol(bohio_proto::Error::Decode)));
                    }
                    (bytes.len() as u64 == head.length).then(|| {
                        Ok(Image {
                            header: head.clone(),
                            bytes: std::mem::take(&mut bytes),
                        })
                    })
                }
                _ => None,
            },
        )
    }

    /// Applies `events` in order; how many the desktop took.
    ///
    /// # Errors
    ///
    /// As [`Gui::snapshot`]; `unavailable` while the screen is locked or the session is not at the screen.
    pub fn input(&self, events: &[InputEvent], timeout: Option<Duration>) -> Result<u32, Error> {
        let seq = self.seq();
        let frame = GuiInput::Input(Input {
            seq,
            events: events.to_vec(),
        });
        self.ask(seq, &frame, timeout, |output| match output {
            GuiOutput::InputDone(done) if done.seq == seq => Some(Ok(done.applied)),
            _ => None,
        })
    }

    /// Waits on the host for the screen to stop changing.
    ///
    /// # Errors
    ///
    /// As [`Gui::snapshot`].
    pub fn settle(
        &self,
        request: &Settle,
        timeout: Option<Duration>,
    ) -> Result<SettleResult, Error> {
        let seq = self.seq();
        let frame = GuiInput::Settle(Settle {
            seq,
            ..request.clone()
        });
        self.ask(seq, &frame, timeout, |output| match output {
            GuiOutput::Settled(done) if done.seq == seq => Some(Ok(SettleResult {
                settled: done.settled,
                waited_ms: done.waited_ms,
            })),
            _ => None,
        })
    }

    /// The desktop clipboard text.
    ///
    /// # Errors
    ///
    /// As [`Gui::snapshot`]; `denied` without `gui.clipboard`.
    pub fn clipboard(&self, timeout: Option<Duration>) -> Result<String, Error> {
        let seq = self.seq();
        self.ask(
            seq,
            &GuiInput::ClipboardGet(ClipboardGet { seq }),
            timeout,
            |output| match output {
                GuiOutput::Clipboard(text) if text.seq == seq => Some(Ok(text.text)),
                _ => None,
            },
        )
    }

    /// Replaces the desktop clipboard text.
    ///
    /// # Errors
    ///
    /// As [`Gui::clipboard`].
    pub fn set_clipboard(&self, text: &str, timeout: Option<Duration>) -> Result<(), Error> {
        let seq = self.seq();
        let frame = GuiInput::ClipboardSet(ClipboardSet {
            seq,
            text: text.to_owned(),
        });
        self.ask(seq, &frame, timeout, |output| match output {
            GuiOutput::Clipboard(done) if done.seq == seq => Some(Ok(())),
            _ => None,
        })
    }

    /// Closes the stream.
    pub fn close(&self) {
        self.channel.close();
    }
}

impl Drop for Gui {
    fn drop(&mut self) {
        self.channel.close();
    }
}

/// The pointer position of a snapshot, when the host reported one.
pub fn pointer(image: &Image) -> Option<Point> {
    image.header.cursor
}
