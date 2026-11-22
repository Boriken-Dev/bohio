//! The desktop a host serves: what a backend (or an application) implements, and what it hands the engine back.

use bohio_proto::gui::{Display, DisplayId, InputEvent, Rect, State};
use bohio_proto::{ErrorBody, ErrorCode};

/// Why a desktop could not do what the engine asked. The message is shown to the peer, so it holds no typed text,
/// clipboard content or path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct GuiError {
    /// What kind of failure.
    pub code: ErrorCode,
    /// A sentence for a person.
    pub message: String,
}

impl GuiError {
    /// A failure of `code`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The desktop exists but cannot be used now: locked, not at the screen, an operating system permission missing.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, message)
    }

    /// The desktop cannot do this at all.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, message)
    }

    /// The thing asked for is not there.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    /// The operating system refused.
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Io, message)
    }

    /// The request names something impossible.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }
}

impl From<GuiError> for ErrorBody {
    fn from(error: GuiError) -> Self {
        Self::new(error.code, error.message)
    }
}

/// What the desktop is doing now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DesktopState {
    /// The account that owns the session, when the backend knows; only a peer holding `gui.view` or `gui.input` is told.
    pub user: Option<String>,
    /// Whether the session is the one at the screen.
    pub active: bool,
    /// Whether the screen is locked; `None` when the backend cannot tell.
    pub locked: Option<bool>,
    /// The displays and their layout; the first with `primary` is the default.
    pub displays: Vec<Display>,
}

impl DesktopState {
    /// The state the wire carries (no account).
    pub fn wire(&self) -> State {
        State {
            active: self.active,
            locked: self.locked,
            displays: self.displays.clone(),
        }
    }

    /// The display a request names, or the primary one (the first when none is marked) when it names none.
    ///
    /// # Errors
    ///
    /// `not_found` for a name no display has, or a desktop with no display.
    pub fn display(&self, id: Option<&DisplayId>) -> Result<&Display, GuiError> {
        let found = match id {
            Some(id) => self.displays.iter().find(|display| &display.id == id),
            None => self
                .displays
                .iter()
                .find(|display| display.primary)
                .or_else(|| self.displays.first()),
        };
        found.ok_or_else(|| GuiError::not_found("no such display"))
    }
}

/// A picture of a display or of a region of one: RGB, three bytes a pixel, rows from the top, no padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

impl Frame {
    /// A frame of `width` by `height` pixels.
    ///
    /// # Errors
    ///
    /// `internal` when `rgb` is not exactly `width * height * 3` bytes, or the frame has no pixel.
    pub fn new(width: u32, height: u32, rgb: Vec<u8>) -> Result<Self, GuiError> {
        let expected = u64::from(width) * u64::from(height) * 3;
        if width == 0 || height == 0 || rgb.len() as u64 != expected {
            return Err(GuiError::new(
                ErrorCode::Internal,
                "the desktop returned a picture that does not match its size",
            ));
        }
        Ok(Self { width, height, rgb })
    }

    /// Width in pixels.
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The pixels, `width * height * 3` bytes.
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }

    /// The pixels, taken.
    pub fn into_rgb(self) -> Vec<u8> {
        self.rgb
    }
}

/// What a desktop can do, as `info` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The backend's name.
    pub backend: String,
    /// Keyboard and mouse input are possible.
    pub input: bool,
    /// The clipboard's text is reachable.
    pub clipboard: bool,
    /// The backend can tell a locked screen.
    pub lock_detection: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            backend: "custom".to_owned(),
            input: true,
            clipboard: true,
            lock_detection: false,
        }
    }
}

/// A desktop session a node serves: an application's own, or a platform backend.
///
/// The engine calls it one call at a time (platform input is not concurrent), from the thread of the stream that asked,
/// and checks every request before it gets here: the grant, the display, the region and the rates. An implementation
/// reports what it cannot do as a [`GuiError`] and never panics on a request.
pub trait Desktop: Send + Sync {
    /// The session's state: who owns it, whether it is at the screen and locked, and its displays.
    ///
    /// # Errors
    ///
    /// `unavailable` or `unsupported` with the reason when there is no desktop to describe.
    fn state(&self) -> Result<DesktopState, GuiError>;

    /// A picture of `region` of `display` (all of it when `None`); the region lies inside the display.
    ///
    /// # Errors
    ///
    /// `unavailable` (a permission of the operating system is missing, the session is not at the screen) or `io`.
    fn capture(&self, display: &DisplayId, region: Option<Rect>) -> Result<Frame, GuiError>;

    /// Applies `events` in order and returns how many it took. A `wait` never reaches it: the engine pauses between the
    /// runs of the other events itself. Coordinates are pixels of the event's display (the primary one by default).
    ///
    /// # Errors
    ///
    /// `unavailable` when input is blocked (another desktop has it, a higher-integrity window has focus), `unsupported`
    /// for an event the backend cannot make.
    fn input(&self, events: &[InputEvent]) -> Result<u32, GuiError>;

    /// The pointer: its display and its position on it.
    fn cursor(&self) -> Option<(DisplayId, i32, i32)>;

    /// The clipboard's text.
    ///
    /// # Errors
    ///
    /// `unsupported` when the backend has no clipboard, `unavailable` when it holds something that is not text.
    fn clipboard_text(&self) -> Result<String, GuiError>;

    /// Replaces the clipboard's text.
    ///
    /// # Errors
    ///
    /// As [`Desktop::clipboard_text`].
    fn set_clipboard_text(&self, text: &str) -> Result<(), GuiError>;

    /// What the backend can do; a custom desktop that does everything needs not say.
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
}
