//! A host's desktop: `bohio-gui/1`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bohio_proto::gui::{
    Ack, ClipboardGet, ClipboardSet, CursorFrame, DisplayId, GuiInput, GuiOutput, GuiRequest,
    ImageFormat, ImageHeader, Input, InputEvent, Point, Ready, Rect, Settle, SettleOptions,
    Snapshot as Wire, State, Unwatch, UpdateHeader, Want, Watch, WatchEncoding,
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

/// What a watch asks for: every field is optional and the host fills what is absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WatchOptions {
    /// The display; the primary one when absent.
    pub display: Option<DisplayId>,
    /// The part of the display; all of it when absent.
    pub region: Option<Rect>,
    /// Updates a second at most; the host's `default_fps` when absent.
    pub fps: Option<u32>,
    /// How rectangles are encoded; the host decides per rectangle when absent.
    pub encoding: Option<WatchEncoding>,
    /// The JPEG quality a rectangle starts at, 1 to 100.
    pub quality: Option<u8>,
    /// Scale the frame down to fit this width.
    pub max_width: Option<u32>,
    /// Scale the frame down to fit this height.
    pub max_height: Option<u32>,
    /// Bytes a second at most.
    pub max_rate: Option<u64>,
    /// Report where the pointer is.
    pub cursor: bool,
    /// Resend lossy rectangles lossless once they stay unchanged.
    pub refine: Option<bool>,
    /// Start with a key frame again.
    pub key: bool,
}

/// What a watch's stream says next.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchEvent {
    /// An update, applied to [`Gui::screen`] before this is returned.
    Update(UpdateInfo),
    /// Only the pointer moved.
    Cursor(CursorFrame),
}

/// An update that was applied.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateInfo {
    /// What the host said about it: the frame's size, the rectangles, the pointer, the host's level and ack delay.
    pub header: UpdateHeader,
    /// How many bytes its rectangles took.
    pub bytes: u64,
    /// Whether this side acknowledged it (see [`Gui::set_auto_ack`]).
    pub acknowledged: bool,
}

/// The frame a watch has built so far: RGB, three bytes a pixel, rows from the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 3` bytes.
    pub rgb: Vec<u8>,
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
    /// A header whose bytes are still arriving.
    assembling: Mutex<Option<Assembly>>,
    auto_ack: AtomicBool,
    updates: AtomicU64,
    /// The most bytes of one update this side reads.
    update_limit: AtomicU64,
    #[cfg(feature = "screen")]
    screen: Mutex<bohio_transfer::screen::Compositor>,
}

/// A header that is waiting for its bytes.
#[derive(Debug)]
enum Assembly {
    Image(ImageHeader, Vec<u8>),
    Update(UpdateHeader, Vec<u8>, u64),
}

/// One complete thing the host sent.
#[derive(Debug)]
enum Item {
    /// A frame that is not an image or an update.
    Frame(GuiOutput),
    /// An image and its bytes.
    Image(ImageHeader, Vec<u8>),
    /// An update and its bytes.
    Update(UpdateHeader, Vec<u8>),
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
                        assembling: Mutex::new(None),
                        auto_ack: AtomicBool::new(true),
                        updates: AtomicU64::new(0),
                        update_limit: AtomicU64::new(DEFAULT_UPDATE_BYTES),
                        #[cfg(feature = "screen")]
                        screen: Mutex::new(bohio_transfer::screen::Compositor::new(
                            bohio_transfer::screen::Limits::default(),
                        )),
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

    /// Reads frames until one complete thing arrived: an image or an update with all its bytes, or any other frame. A
    /// header whose bytes did not all arrive in time stays, and the next read goes on with it.
    fn read(&self, left: Option<Duration>) -> Result<Item, Error> {
        let deadline = left.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            let Some(bytes) = self.channel.receive(left)? else {
                return Err(Error::Closed);
            };
            let mut assembling = self
                .assembling
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match GuiOutput::decode(&bytes) {
                Ok(GuiOutput::Image(header)) => {
                    if header.length == 0 {
                        return Ok(Item::Image(header, Vec::new()));
                    }
                    *assembling = Some(Assembly::Image(header, Vec::new()));
                }
                Ok(GuiOutput::Update(header)) => {
                    let total = header
                        .rects
                        .iter()
                        .fold(0_u64, |sum, part| sum.saturating_add(part.length));
                    if total > self.update_limit.load(Ordering::Relaxed) {
                        return Err(Error::Screen(
                            "an update holds more bytes than this side accepts".to_owned(),
                        ));
                    }
                    if total == 0 {
                        return Ok(Item::Update(header, Vec::new()));
                    }
                    *assembling = Some(Assembly::Update(header, Vec::new(), total));
                }
                Ok(GuiOutput::Data(piece)) => match assembling.take() {
                    Some(Assembly::Image(header, mut bytes)) => {
                        bytes.extend_from_slice(&piece);
                        if bytes.len() as u64 > header.length {
                            return Err(Error::Protocol(bohio_proto::Error::Decode));
                        }
                        if bytes.len() as u64 == header.length {
                            return Ok(Item::Image(header, bytes));
                        }
                        *assembling = Some(Assembly::Image(header, bytes));
                    }
                    Some(Assembly::Update(header, mut bytes, total)) => {
                        bytes.extend_from_slice(&piece);
                        if bytes.len() as u64 > total {
                            return Err(Error::Protocol(bohio_proto::Error::Decode));
                        }
                        if bytes.len() as u64 == total {
                            return Ok(Item::Update(header, bytes));
                        }
                        *assembling = Some(Assembly::Update(header, bytes, total));
                    }
                    None => {}
                },
                Ok(output) => return Ok(Item::Frame(output)),
                Err(bohio_proto::Error::Tag(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Applies an update to the frame and acknowledges it unless acknowledgements are off; whether it was acknowledged.
    fn apply(&self, header: &UpdateHeader, bytes: &[u8]) -> Result<bool, Error> {
        #[cfg(feature = "screen")]
        self.screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .apply(header, bytes)
            .map_err(|error| Error::Screen(error.to_string()))?;
        #[cfg(not(feature = "screen"))]
        let _ = bytes;
        self.updates.fetch_add(1, Ordering::Relaxed);
        if self.auto_ack.load(Ordering::Relaxed) {
            self.ack(header.frame)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Sends `frame` and passes each answer to `take` until it returns a value; `state` frames are kept, updates are applied
    /// and acknowledged, and frames of another request are skipped.
    fn ask<T>(
        &self,
        seq: u64,
        frame: &GuiInput,
        timeout: Option<Duration>,
        mut take: impl FnMut(Item) -> Option<Result<T, Error>>,
    ) -> Result<T, Error> {
        let _turn = self
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = timeout.map(|limit| Instant::now() + limit);
        self.channel.send_timeout(&frame.encode()?, timeout)?;
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            match self.read(left)? {
                Item::Frame(GuiOutput::State(state)) => {
                    *self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
                }
                Item::Frame(GuiOutput::RequestError(failure))
                    if failure.seq == seq || failure.seq == 0 =>
                {
                    return Err(Error::Remote(failure.body()));
                }
                Item::Frame(GuiOutput::Error(body)) => return Err(Error::Remote(body)),
                Item::Update(header, bytes) => {
                    self.apply(&header, &bytes)?;
                }
                item => {
                    if let Some(done) = take(item) {
                        return done;
                    }
                }
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
        self.ask(seq, &GuiInput::Snapshot(wire), timeout, |item| match item {
            Item::Image(header, bytes) if header.seq == seq => Some(Ok(Image { header, bytes })),
            _ => None,
        })
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
        self.ask(seq, &frame, timeout, |item| match item {
            Item::Frame(GuiOutput::InputDone(done)) if done.seq == seq => Some(Ok(done.applied)),
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
        self.ask(seq, &frame, timeout, |item| match item {
            Item::Frame(GuiOutput::Settled(done)) if done.seq == seq => Some(Ok(SettleResult {
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
            |item| match item {
                Item::Frame(GuiOutput::Clipboard(text)) if text.seq == seq => Some(Ok(text.text)),
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
        self.ask(seq, &frame, timeout, |item| match item {
            Item::Frame(GuiOutput::Clipboard(done)) if done.seq == seq => Some(Ok(())),
            _ => None,
        })
    }

    /// Holds updates to `max_rect_bytes` for one rectangle (and for the rectangles of one update together) and to
    /// `max_frame_pixels` for the frame they build: what a host that sends more is refused with, so it cannot grow this
    /// side. The defaults are `[gui] max_rect_bytes` and `max_snapshot_pixels` of a host that sets nothing.
    #[cfg(feature = "screen")]
    pub fn set_screen_limits(&self, max_rect_bytes: u64, max_frame_pixels: u64) {
        self.update_limit.store(max_rect_bytes, Ordering::Relaxed);
        self.screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_limits(bohio_transfer::screen::Limits {
                max_rect_bytes,
                max_frame_pixels,
            });
    }

    /// How many updates this side has applied.
    pub fn updates(&self) -> u64 {
        self.updates.load(Ordering::Relaxed)
    }

    /// Whether an update is acknowledged as soon as it is applied (the default). Without it the host stops after
    /// `frames_in_flight` updates until [`Gui::ack`] says each is applied.
    pub fn set_auto_ack(&self, on: bool) {
        self.auto_ack.store(on, Ordering::Relaxed);
    }

    /// Acknowledges the update `frame`.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] or [`Error::Timeout`] when the frame cannot be sent.
    pub fn ack(&self, frame: u64) -> Result<(), Error> {
        self.channel
            .send_timeout(&GuiInput::Ack(Ack { frame }).encode()?, Some(OPEN_WAIT))?;
        Ok(())
    }

    /// Starts a watch of the screen, or replaces the settings of the running one. Updates arrive through
    /// [`Gui::next_event`].
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] or [`Error::Timeout`] when the frame cannot be sent; the host's refusal (`denied`,
    /// `bad_request`...) arrives as [`Error::Remote`] from the next [`Gui::next_event`].
    pub fn watch(&self, options: &WatchOptions) -> Result<(), Error> {
        let seq = self.seq();
        let frame = GuiInput::Watch(Watch {
            seq,
            display: options.display.clone(),
            region: options.region,
            fps: options.fps,
            encoding: options.encoding,
            quality: options.quality,
            max_width: options.max_width,
            max_height: options.max_height,
            max_rate: options.max_rate,
            cursor: options.cursor,
            refine: options.refine,
            key: options.key,
        });
        let _turn = self
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.channel
            .send_timeout(&frame.encode()?, Some(OPEN_WAIT))?;
        Ok(())
    }

    /// Stops the watch; updates already sent may still arrive.
    ///
    /// # Errors
    ///
    /// As [`Gui::watch`].
    pub fn unwatch(&self) -> Result<(), Error> {
        let seq = self.seq();
        let _turn = self
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.channel.send_timeout(
            &GuiInput::Unwatch(Unwatch { seq }).encode()?,
            Some(OPEN_WAIT),
        )?;
        Ok(())
    }

    /// The next thing a watch says: an update (already applied to [`Gui::screen`]) or a pointer move. Frames of other
    /// kinds are handled and skipped.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] when nothing came within `timeout` (call again), [`Error::Remote`] when the host refused or ended
    /// the watch, [`Error::Screen`] for an update that cannot be used, [`Error::Closed`].
    pub fn next_event(&self, timeout: Option<Duration>) -> Result<WatchEvent, Error> {
        let _turn = self
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            match self.read(left)? {
                Item::Frame(GuiOutput::State(state)) => {
                    *self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
                }
                Item::Frame(GuiOutput::Cursor(cursor)) => return Ok(WatchEvent::Cursor(cursor)),
                Item::Frame(GuiOutput::RequestError(failure)) => {
                    return Err(Error::Remote(failure.body()));
                }
                Item::Frame(GuiOutput::Error(body)) => return Err(Error::Remote(body)),
                Item::Update(header, bytes) => {
                    let acknowledged = self.apply(&header, &bytes)?;
                    return Ok(WatchEvent::Update(UpdateInfo {
                        bytes: bytes.len() as u64,
                        header,
                        acknowledged,
                    }));
                }
                Item::Frame(_) | Item::Image(..) => {}
            }
        }
    }

    /// The frame the updates have built, `None` before the first one (and always without the `screen` feature, which
    /// builds the frame).
    pub fn screen(&self) -> Option<Screen> {
        #[cfg(feature = "screen")]
        {
            let screen = self
                .screen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            screen.frame().map(|pixels| Screen {
                width: pixels.width(),
                height: pixels.height(),
                rgb: pixels.rgb().to_vec(),
            })
        }
        #[cfg(not(feature = "screen"))]
        None
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

/// What a rectangle, and an update's rectangles together, may hold when the caller sets no limit: `[gui] max_rect_bytes`.
const DEFAULT_UPDATE_BYTES: u64 = 32 * 1024 * 1024;

/// The pointer position of a snapshot, when the host reported one.
pub fn pointer(image: &Image) -> Option<Point> {
    image.header.cursor
}
