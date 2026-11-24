//! The screen of a host's desktop: the frames of `bohio-gui/1`, and what `info` says of a desktop.
//!
//! `bohio-gui/1`: the open's metadata is a [`GuiRequest`]. The host answers with [`GuiOutput::Ready`] (what the grant
//! allows and the desktop's state), or [`GuiOutput::Error`] and the end. The client then sends requests, each with a
//! `seq` of its own choosing, and the host answers each with the frames below carrying the same `seq`; a request that
//! fails is a [`GuiOutput::RequestError`] and the stream goes on. Coordinates are physical pixels of one display with its
//! top-left corner as the origin; `display` names it and defaults to the primary one.
//!
//! An image is a [`GuiOutput::Image`] header followed by [`GuiOutput::Data`] frames that carry exactly `length` bytes
//! between them.
//!
//! | Direction | Tag | Frame | Body |
//! | --- | --- | --- | --- |
//! | host to client | `0x01` | [`GuiOutput::Ready`] | CBOR [`Ready`] |
//! | host to client | `0x02` | [`GuiOutput::Image`] | CBOR [`ImageHeader`] |
//! | host to client | `0x03` | [`GuiOutput::Data`] | the image's bytes, raw |
//! | host to client | `0x04` | [`GuiOutput::Update`] | CBOR [`UpdateHeader`], then `0x03` frames |
//! | host to client | `0x05` | [`GuiOutput::Error`] | CBOR [`ErrorBody`]; the last frame |
//! | host to client | `0x06` | [`GuiOutput::InputDone`] | CBOR [`InputDone`] |
//! | host to client | `0x07` | [`GuiOutput::Settled`] | CBOR [`SettledFrame`] |
//! | host to client | `0x08` | [`GuiOutput::Clipboard`] | CBOR [`ClipboardText`] |
//! | host to client | `0x09` | [`GuiOutput::State`] | CBOR [`State`], whenever it changes |
//! | host to client | `0x0a` | [`GuiOutput::Cursor`] | CBOR [`CursorFrame`] |
//! | host to client | `0x0b` | [`GuiOutput::RequestError`] | CBOR [`RequestError`] |
//! | client to host | `0x10` | [`GuiInput::Snapshot`] | CBOR [`Snapshot`] |
//! | client to host | `0x11` | [`GuiInput::Watch`] | CBOR [`Watch`] |
//! | client to host | `0x12` | [`GuiInput::Unwatch`] | CBOR [`Unwatch`] |
//! | client to host | `0x13` | [`GuiInput::Ack`] | CBOR [`Ack`] |
//! | client to host | `0x14` | [`GuiInput::Input`] | CBOR [`Input`] |
//! | client to host | `0x15` | [`GuiInput::Settle`] | CBOR [`Settle`] |
//! | client to host | `0x16` | [`GuiInput::ClipboardGet`] | CBOR [`ClipboardGet`] |
//! | client to host | `0x17` | [`GuiInput::ClipboardSet`] | CBOR [`ClipboardSet`] |
//!
//! `bohio-gui-approve/1` is served on a node's local endpoint only: [`ApproveOutput`] and [`ApproveInput`] are its
//! messages, CBOR maps like the other local protocols.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorBody, ErrorCode};
use crate::{decode, encode};

/// One display of a desktop, named by the host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DisplayId(pub String);

impl DisplayId {
    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisplayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for DisplayId {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

/// What a peer asks to do on a desktop: each is a permission of its own on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Want {
    /// Snapshots, and the settle wait.
    View,
    /// Keyboard, mouse and scroll.
    Input,
    /// Reading and writing the clipboard's text.
    Clipboard,
}

impl Want {
    /// The name on the wire.
    pub const fn name(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Input => "input",
            Self::Clipboard => "clipboard",
        }
    }
}

impl fmt::Display for Want {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The open's metadata of `bohio-gui/1`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuiRequest {
    /// The desktop session; the host's only one when absent.
    #[serde(default)]
    pub session: Option<String>,
    /// What the stream is for: `view` when absent, never empty. A wanted item the peer's grant lacks is `denied`.
    #[serde(default)]
    pub want: Option<Vec<Want>>,
    /// The desktop source; the node's default one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl GuiRequest {
    /// A request for `want`.
    pub fn new(want: impl IntoIterator<Item = Want>) -> Self {
        Self {
            session: None,
            want: Some(want.into_iter().collect()),
            source: None,
        }
    }

    /// What the stream is for, once the default is applied (sorted, without repeats).
    pub fn wants(&self) -> Vec<Want> {
        let mut wants = self.want.clone().unwrap_or_else(|| vec![Want::View]);
        wants.sort();
        wants.dedup();
        wants
    }

    /// The open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when the request cannot be written.
    pub fn to_metadata(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a request from an open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not a request.
    pub fn from_metadata(metadata: &[u8]) -> Result<Self, Error> {
        decode(metadata)
    }
}

/// A rectangle of pixels: `x` and `y` from the top-left of a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Rect {
    /// The rectangle at `x`, `y` of `width` by `height`.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Whether the rectangle lies inside a display of `width` by `height` and has an area.
    pub fn fits(&self, width: u32, height: u32) -> bool {
        self.width > 0
            && self.height > 0
            && u64::from(self.x) + u64::from(self.width) <= u64::from(width)
            && u64::from(self.y) + u64::from(self.height) <= u64::from(height)
    }

    /// How many pixels it holds.
    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// A point on a display, in its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    /// Distance from the left edge.
    pub x: i32,
    /// Distance from the top edge.
    pub y: i32,
}

/// One display of a desktop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Display {
    /// How requests name it.
    pub id: DisplayId,
    /// A name for a person.
    #[serde(default)]
    pub name: Option<String>,
    /// The display a request without `display` means.
    #[serde(default)]
    pub primary: bool,
    /// Its left edge in the desktop's layout.
    pub x: i32,
    /// Its top edge in the desktop's layout.
    pub y: i32,
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
    /// The display's scale factor (physical pixels per logical point).
    #[serde(default = "one")]
    pub scale: f64,
    /// Its rotation in degrees (0, 90, 180 or 270), when the host knows.
    #[serde(default)]
    pub rotation: Option<u32>,
}

fn one() -> f64 {
    1.0
}

/// What the desktop is doing now: the `state` frame, and what a session of `info` carries.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Whether the session is the one at the screen. Input is refused while it is not.
    #[serde(default)]
    pub active: bool,
    /// Whether the screen is locked; absent when the host cannot tell.
    #[serde(default)]
    pub locked: Option<bool>,
    /// The displays, with their layout.
    #[serde(default)]
    pub displays: Vec<Display>,
}

/// The host's first frame: the stream is open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ready {
    /// The desktop session served.
    pub session: String,
    /// What the grant allows of what was asked; every other request of that kind is `denied`.
    pub granted: Vec<Want>,
    /// The desktop as it is now.
    pub state: State,
}

/// The formats an image is encoded in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    /// Lossless; text stays legible.
    Png,
    /// Lossy, with a quality of 1 to 100.
    Jpeg,
    /// Lossless WebP.
    Webp,
}

impl ImageFormat {
    /// Every format this version names.
    pub const ALL: [Self; 3] = [Self::Png, Self::Jpeg, Self::Webp];

    /// The name on the wire and in `bohio.cfg`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
        }
    }

    /// The format named `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.name() == name)
    }
}

impl fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How the host waits for a screen to stop changing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettleOptions {
    /// How long the screen must stay unchanged, in milliseconds; the host's `settle_quiet` when absent.
    #[serde(default)]
    pub quiet_ms: Option<u64>,
    /// How long to wait at most, in milliseconds; the host's `settle_timeout` when absent.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// A settle's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settled {
    /// The screen stayed unchanged for the quiet time; `false` when the timeout came first.
    pub settled: bool,
    /// How long the host waited, in milliseconds.
    pub waited_ms: u64,
}

/// The client's `0x10` frame: capture a display or a region of it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// Matches the answer.
    pub seq: u64,
    /// The display; the primary one when absent.
    #[serde(default)]
    pub display: Option<DisplayId>,
    /// The part of the display; all of it when absent.
    #[serde(default)]
    pub region: Option<Rect>,
    /// The format; the host's first allowed one when absent.
    #[serde(default)]
    pub format: Option<ImageFormat>,
    /// JPEG quality, 1 to 100; the host's `jpeg_quality` when absent.
    #[serde(default)]
    pub quality: Option<u8>,
    /// The image is scaled down, keeping its proportions, to fit this width; never scaled up.
    #[serde(default)]
    pub max_width: Option<u32>,
    /// As `max_width`, for the height.
    #[serde(default)]
    pub max_height: Option<u32>,
    /// Report where the pointer is.
    #[serde(default)]
    pub cursor: bool,
    /// Wait for the screen to settle first.
    #[serde(default)]
    pub settle: Option<SettleOptions>,
}

/// The host's `0x02` frame: an image whose bytes follow in `0x03` frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageHeader {
    /// The request's `seq`.
    pub seq: u64,
    /// The display captured.
    pub display: DisplayId,
    /// The image's format.
    pub format: ImageFormat,
    /// The image's width in pixels, after any scaling.
    pub width: u32,
    /// The image's height in pixels, after any scaling.
    pub height: u32,
    /// What part of the display the image shows, in the display's pixels.
    pub region: Rect,
    /// Image pixels per display pixel: `1` unless the image was scaled down.
    pub scale: f64,
    /// Where the pointer is, in the display's pixels, when asked for and it is on this display.
    #[serde(default)]
    pub cursor: Option<Point>,
    /// How many bytes the `0x03` frames carry.
    pub length: u64,
    /// The settle's outcome, when the snapshot asked for one.
    #[serde(default)]
    pub settled: Option<Settled>,
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    /// The primary button.
    Left,
    /// The secondary button.
    Right,
    /// The wheel's button.
    Middle,
    /// The back button.
    Back,
    /// The forward button.
    Forward,
}

/// What a button does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonAction {
    /// Go down and hold.
    Press,
    /// Let go.
    Release,
    /// Press and release, `count` times.
    #[default]
    Click,
}

/// What a key does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    /// Go down and hold.
    Press,
    /// Let go.
    Release,
    /// Press and release.
    #[default]
    Click,
}

/// The key names a `key` event takes besides one character, in snake case.
pub const KEY_NAMES: &[&str] = &[
    "enter",
    "tab",
    "escape",
    "backspace",
    "delete",
    "insert",
    "home",
    "end",
    "page_up",
    "page_down",
    "up",
    "down",
    "left",
    "right",
    "space",
    "shift",
    "control",
    "alt",
    "meta",
    "caps_lock",
    "print_screen",
    "menu",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
    "f13",
    "f14",
    "f15",
    "f16",
    "f17",
    "f18",
    "f19",
    "f20",
    "f21",
    "f22",
    "f23",
    "f24",
];

/// Whether `key` is a name a `key` event takes: one of [`KEY_NAMES`], or exactly one character.
pub fn is_key(key: &str) -> bool {
    let mut characters = key.chars();
    match (characters.next(), characters.next()) {
        (Some(_), None) => true,
        (Some(_), Some(_)) => !key.is_empty() && KEY_NAMES.contains(&key),
        _ => false,
    }
}

/// One input event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputEvent {
    /// Moves the pointer.
    Move {
        /// The display; the primary one when absent.
        #[serde(default)]
        display: Option<DisplayId>,
        /// Pixels from the display's left edge.
        x: i32,
        /// Pixels from the display's top edge.
        y: i32,
    },
    /// Presses, releases or clicks a button, moving the pointer first when `x` and `y` are given.
    Button {
        /// Which button.
        button: Button,
        /// What it does; a click when absent.
        #[serde(default)]
        action: ButtonAction,
        /// How many clicks (a double click is 2); `1` when absent.
        #[serde(default)]
        count: Option<u32>,
        /// The display `x` and `y` are on.
        #[serde(default)]
        display: Option<DisplayId>,
        /// Where to move first.
        #[serde(default)]
        x: Option<i32>,
        /// Where to move first.
        #[serde(default)]
        y: Option<i32>,
    },
    /// Turns the wheel, in notches: a positive `dy` scrolls the content down, a positive `dx` to the right.
    Scroll {
        /// The display `x` and `y` are on.
        #[serde(default)]
        display: Option<DisplayId>,
        /// Where to move first.
        #[serde(default)]
        x: Option<i32>,
        /// Where to move first.
        #[serde(default)]
        y: Option<i32>,
        /// Horizontal notches.
        #[serde(default)]
        dx: i32,
        /// Vertical notches.
        #[serde(default)]
        dy: i32,
    },
    /// Presses, releases or clicks a key: a name of [`KEY_NAMES`] or one character.
    Key {
        /// Which key.
        key: String,
        /// What it does; a click when absent.
        #[serde(default)]
        action: KeyAction,
    },
    /// Types text as it is, whatever the keyboard layout.
    Text {
        /// The characters.
        text: String,
    },
    /// Pauses before the next event of a batch.
    Wait {
        /// Milliseconds.
        ms: u64,
    },
}

/// Text that is not a key combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadCombo(pub String);

impl fmt::Display for BadCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` is not a key", self.0)
    }
}

impl core::error::Error for BadCombo {}

/// The wire name of one key as xdotool and people spell it, `None` when it is not a key.
fn key_name(part: &str) -> Option<String> {
    let lower = part.to_lowercase();
    let name = match lower.as_str() {
        "ctrl" | "control" | "control_l" | "control_r" => "control",
        "shift" | "shift_l" | "shift_r" => "shift",
        "alt" | "alt_l" | "alt_r" | "option" => "alt",
        "super" | "cmd" | "command" | "win" | "windows" | "meta" | "super_l" | "super_r" => "meta",
        "return" | "enter" | "kp_enter" => "enter",
        "escape" | "esc" => "escape",
        "page_down" | "pagedown" | "next" => "page_down",
        "page_up" | "pageup" | "prior" => "page_up",
        "del" | "delete" => "delete",
        "capslock" => "caps_lock",
        "print" => "print_screen",
        other if KEY_NAMES.contains(&other) => other,
        _ => {
            let mut characters = part.chars();
            return match (characters.next(), characters.next()) {
                (Some(_), None) => Some(part.to_owned()),
                _ => None,
            };
        }
    };
    Some(name.to_owned())
}

impl InputEvent {
    /// A key combination as people write it (`ctrl+shift+t`, `Return`, `alt+F4`, `super`): every key but the last is held
    /// down while the last is clicked, then released in reverse. Names follow xdotool: `Return`, `BackSpace`, `Escape`,
    /// `Page_Down`, `ctrl`, `super`, `cmd` and `win` (the last three are `meta`).
    ///
    /// # Errors
    ///
    /// [`BadCombo`] naming the part that is not a key.
    pub fn combo(text: &str) -> Result<Vec<Self>, BadCombo> {
        let parts: Vec<&str> = if text == "+" {
            vec!["+"]
        } else {
            text.split('+').collect()
        };
        let mut keys = Vec::new();
        for part in &parts {
            keys.push(key_name(part).ok_or_else(|| BadCombo((*part).to_owned()))?);
        }
        let last = keys.pop().ok_or_else(|| BadCombo(text.to_owned()))?;
        let key = |key: String, action| Self::Key { key, action };
        let mut events: Vec<Self> = keys
            .iter()
            .map(|held| key(held.clone(), KeyAction::Press))
            .collect();
        events.push(key(last, KeyAction::Click));
        events.extend(
            keys.into_iter()
                .rev()
                .map(|held| key(held, KeyAction::Release)),
        );
        Ok(events)
    }
}

/// The client's `0x14` frame: events, applied in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    /// Matches the answer.
    pub seq: u64,
    /// The events; at least one.
    pub events: Vec<InputEvent>,
}

/// The host's `0x06` frame: how many events the desktop took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDone {
    /// The request's `seq`.
    pub seq: u64,
    /// How many events were applied (a `wait` is applied once it has passed).
    pub applied: u32,
}

/// The client's `0x15` frame: wait for the screen to stop changing.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Settle {
    /// Matches the answer.
    pub seq: u64,
    /// The display; the primary one when absent.
    #[serde(default)]
    pub display: Option<DisplayId>,
    /// The part of the display watched; all of it when absent.
    #[serde(default)]
    pub region: Option<Rect>,
    /// How long it must stay unchanged, in milliseconds; the host's `settle_quiet` when absent.
    #[serde(default)]
    pub quiet_ms: Option<u64>,
    /// How long to wait at most, in milliseconds; the host's `settle_timeout` when absent.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// The host's `0x07` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettledFrame {
    /// The request's `seq`.
    pub seq: u64,
    /// The screen stayed unchanged for the quiet time; `false` when the timeout came first.
    pub settled: bool,
    /// How long the host waited, in milliseconds.
    pub waited_ms: u64,
}

/// The client's `0x16` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardGet {
    /// Matches the answer.
    pub seq: u64,
}

/// The client's `0x17` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardSet {
    /// Matches the answer.
    pub seq: u64,
    /// The text the clipboard then holds.
    pub text: String,
}

/// The host's `0x08` frame: the clipboard's text, or the acknowledgement of a write (the text then empty).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardText {
    /// The request's `seq`.
    pub seq: u64,
    /// The clipboard's text.
    pub text: String,
}

/// What a watch's rectangles are encoded with when the client leaves the choice to the host or pins one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchEncoding {
    /// The host picks per rectangle: lossless when it compresses below `lossless_max_bpp` bytes a pixel, else JPEG; a
    /// rectangle that stays unchanged is later resent lossless.
    #[default]
    Auto,
    /// Every rectangle lossless; the host never lowers quality or size, only the frame rate.
    Lossless,
    /// Every rectangle JPEG.
    Jpeg,
}

/// How one rectangle of an update is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RectEncoding {
    /// RGB24 rows, one zstd frame for the rectangle; lossless.
    ZstdRgb,
    /// RGB24 rows as they are; lossless, for a build without zstd.
    RawRgb,
    /// A JPEG of the rectangle.
    Jpeg,
    /// A PNG of the rectangle; lossless.
    Png,
}

impl RectEncoding {
    /// The name on the wire.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ZstdRgb => "zstd_rgb",
            Self::RawRgb => "raw_rgb",
            Self::Jpeg => "jpeg",
            Self::Png => "png",
        }
    }

    /// Whether decoding gives back exactly what was encoded.
    pub const fn lossless(self) -> bool {
        !matches!(self, Self::Jpeg)
    }
}

impl fmt::Display for RectEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The client's `0x11` frame: send the changes of a display as they happen. A `watch` while one is running replaces its
/// settings (an absent field is the host's default again, not the earlier value), and `key: true` asks for a key frame.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Watch {
    /// Matches an error answer; updates carry their own `frame` numbers.
    pub seq: u64,
    /// The display; the primary one when absent.
    #[serde(default)]
    pub display: Option<DisplayId>,
    /// The part of the display; all of it when absent.
    #[serde(default)]
    pub region: Option<Rect>,
    /// Updates a second at most; the host's `default_fps` when absent, never more than its `max_fps`.
    #[serde(default)]
    pub fps: Option<u32>,
    /// How rectangles are encoded; [`WatchEncoding::Auto`] when absent.
    #[serde(default)]
    pub encoding: Option<WatchEncoding>,
    /// The JPEG quality a rectangle starts at, 1 to 100; the host's `watch_quality` when absent. The host lowers it, never
    /// raises it, to keep up with the link.
    #[serde(default)]
    pub quality: Option<u8>,
    /// The frame is scaled down, keeping its proportions, to fit this width; never scaled up.
    #[serde(default)]
    pub max_width: Option<u32>,
    /// As `max_width`, for the height.
    #[serde(default)]
    pub max_height: Option<u32>,
    /// Bytes a second the updates may add up to, on top of what the link allows; the host's `max_rate` (when set) is a
    /// ceiling for it. Not zero.
    #[serde(default)]
    pub max_rate: Option<u64>,
    /// Report where the pointer is.
    #[serde(default)]
    pub cursor: bool,
    /// Resend lossy rectangles lossless once they stay unchanged; on for `auto` and off for `jpeg` when absent.
    #[serde(default)]
    pub refine: Option<bool>,
    /// Send a key frame (the whole frame) next.
    #[serde(default)]
    pub key: bool,
}

/// The client's `0x12` frame: stop the watch. Updates already sent may still arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unwatch {
    /// Matches an error answer.
    pub seq: u64,
}

/// The client's `0x13` frame: the update `frame` is applied. The host sends at most `frames_in_flight` updates that are
/// not acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    /// The update's `frame`.
    pub frame: u64,
}

/// One rectangle of an update; its bytes are the next `length` bytes of the update's `0x03` frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRect {
    /// Left edge in the frame.
    pub x: u32,
    /// Top edge in the frame.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// How the bytes encode the pixels.
    pub encoding: RectEncoding,
    /// How many bytes the rectangle takes of the update's data.
    pub length: u64,
}

impl UpdateRect {
    /// The rectangle's place in the frame.
    pub const fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }
}

/// The host's `0x04` frame: what changed in the frame of a watch, followed by `0x03` frames that carry the rectangles'
/// bytes in order, exactly the sum of their `length`s. The frame is the display (or region) scaled by `scale`; applying
/// every update in order to the earlier ones gives the screen as it was when the last was captured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateHeader {
    /// This update's number, counting from 1; the client's [`Ack`] names it.
    pub frame: u64,
    /// The display watched.
    pub display: DisplayId,
    /// The frame's width in pixels, after any scaling.
    pub width: u32,
    /// The frame's height in pixels, after any scaling.
    pub height: u32,
    /// Frame pixels per display pixel: `1` unless the host scaled the frame down.
    pub scale: f64,
    /// The rectangles hold the whole frame; the first update, one after the size changed and one asked for with `key`.
    pub key: bool,
    /// The changed rectangles.
    pub rects: Vec<UpdateRect>,
    /// Where the pointer is, in the display's pixels, when asked for and it is on this display.
    #[serde(default)]
    pub cursor: Option<Point>,
    /// The host's quality step: `0` is full quality at the requested rate; each higher step is cheaper.
    #[serde(default)]
    pub level: u32,
    /// The host's average of how long clients took to acknowledge an update, in milliseconds, once it has one.
    #[serde(default)]
    pub ack_ms: Option<u64>,
}

/// The host's `0x0a` frame: only the pointer moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorFrame {
    /// The display the pointer is on.
    pub display: DisplayId,
    /// Pixels from the display's left edge.
    pub x: i32,
    /// Pixels from the display's top edge.
    pub y: i32,
    /// Whether the pointer is on the watched display.
    pub visible: bool,
}

/// The host's `0x0b` frame: one request failed and the stream goes on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestError {
    /// The request's `seq`.
    pub seq: u64,
    /// What kind of failure.
    pub code: ErrorCode,
    /// A sentence for a person; it never carries typed text, a key or the clipboard.
    #[serde(default)]
    pub message: String,
}

impl RequestError {
    /// The failure of request `seq`.
    pub fn new(seq: u64, body: ErrorBody) -> Self {
        Self {
            seq,
            code: body.code,
            message: body.message,
        }
    }

    /// The failure as a plain body.
    pub fn body(&self) -> ErrorBody {
        ErrorBody::new(self.code, self.message.clone())
    }
}

const READY: u8 = 0x01;
const IMAGE: u8 = 0x02;
const DATA: u8 = 0x03;
const UPDATE: u8 = 0x04;
const ERROR: u8 = 0x05;
const INPUT_DONE: u8 = 0x06;
const SETTLED: u8 = 0x07;
const CLIPBOARD: u8 = 0x08;
const STATE: u8 = 0x09;
const CURSOR: u8 = 0x0a;
const REQUEST_ERROR: u8 = 0x0b;
const SNAPSHOT: u8 = 0x10;
const WATCH: u8 = 0x11;
const UNWATCH: u8 = 0x12;
const ACK: u8 = 0x13;
const INPUT: u8 = 0x14;
const SETTLE: u8 = 0x15;
const CLIPBOARD_GET: u8 = 0x16;
const CLIPBOARD_SET: u8 = 0x17;

fn tagged(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(body.len() + 1);
    frame.push(tag);
    frame.extend_from_slice(body);
    frame
}

fn split(frame: &[u8]) -> Result<(u8, &[u8]), Error> {
    frame
        .split_first()
        .map(|(tag, body)| (*tag, body))
        .ok_or(Error::Short)
}

/// A frame of a GUI stream, from the host.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GuiOutput {
    /// The stream is open.
    Ready(Ready),
    /// An image header; its bytes follow.
    Image(ImageHeader),
    /// A piece of the bytes of an image or of an update's rectangles.
    Data(Vec<u8>),
    /// What changed on the screen of a watch; its bytes follow.
    Update(UpdateHeader),
    /// The pointer moved and nothing else did.
    Cursor(CursorFrame),
    /// The stream failed or was refused; the last frame.
    Error(ErrorBody),
    /// Input was applied.
    InputDone(InputDone),
    /// A settle ended.
    Settled(SettledFrame),
    /// The clipboard's text.
    Clipboard(ClipboardText),
    /// The desktop changed.
    State(State),
    /// One request failed.
    RequestError(RequestError),
}

impl GuiOutput {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Ready(body) => tagged(READY, &encode(body)?),
            Self::Image(body) => tagged(IMAGE, &encode(body)?),
            Self::Data(body) => tagged(DATA, body),
            Self::Update(body) => tagged(UPDATE, &encode(body)?),
            Self::Cursor(body) => tagged(CURSOR, &encode(body)?),
            Self::Error(body) => tagged(ERROR, &encode(body)?),
            Self::InputDone(body) => tagged(INPUT_DONE, &encode(body)?),
            Self::Settled(body) => tagged(SETTLED, &encode(body)?),
            Self::Clipboard(body) => tagged(CLIPBOARD, &encode(body)?),
            Self::State(body) => tagged(STATE, &encode(body)?),
            Self::RequestError(body) => tagged(REQUEST_ERROR, &encode(body)?),
        })
    }

    /// Reads one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know,
    /// [`Error::Decode`] for a CBOR body that is not what its tag says.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = split(frame)?;
        match tag {
            READY => decode(body).map(Self::Ready),
            IMAGE => decode(body).map(Self::Image),
            DATA => Ok(Self::Data(body.to_vec())),
            UPDATE => decode(body).map(Self::Update),
            CURSOR => decode(body).map(Self::Cursor),
            ERROR => decode(body).map(Self::Error),
            INPUT_DONE => decode(body).map(Self::InputDone),
            SETTLED => decode(body).map(Self::Settled),
            CLIPBOARD => decode(body).map(Self::Clipboard),
            STATE => decode(body).map(Self::State),
            REQUEST_ERROR => decode(body).map(Self::RequestError),
            other => Err(Error::Tag(other)),
        }
    }
}

/// A frame of a GUI stream, from the client.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GuiInput {
    /// Capture an image.
    Snapshot(Snapshot),
    /// Send the changes of a display as they happen.
    Watch(Watch),
    /// Stop the watch.
    Unwatch(Unwatch),
    /// An update was applied.
    Ack(Ack),
    /// Apply input events.
    Input(Input),
    /// Wait for the screen to settle.
    Settle(Settle),
    /// Read the clipboard.
    ClipboardGet(ClipboardGet),
    /// Write the clipboard.
    ClipboardSet(ClipboardSet),
}

impl GuiInput {
    /// The frame as one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when a CBOR body cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(match self {
            Self::Snapshot(body) => tagged(SNAPSHOT, &encode(body)?),
            Self::Watch(body) => tagged(WATCH, &encode(body)?),
            Self::Unwatch(body) => tagged(UNWATCH, &encode(body)?),
            Self::Ack(body) => tagged(ACK, &encode(body)?),
            Self::Input(body) => tagged(INPUT, &encode(body)?),
            Self::Settle(body) => tagged(SETTLE, &encode(body)?),
            Self::ClipboardGet(body) => tagged(CLIPBOARD_GET, &encode(body)?),
            Self::ClipboardSet(body) => tagged(CLIPBOARD_SET, &encode(body)?),
        })
    }

    /// Reads one stream message.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for an empty message, [`Error::Tag`] for a tag this version does not know,
    /// [`Error::Decode`] for a CBOR body that is not what its tag says.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        let (tag, body) = split(frame)?;
        match tag {
            SNAPSHOT => decode(body).map(Self::Snapshot),
            WATCH => decode(body).map(Self::Watch),
            UNWATCH => decode(body).map(Self::Unwatch),
            ACK => decode(body).map(Self::Ack),
            INPUT => decode(body).map(Self::Input),
            SETTLE => decode(body).map(Self::Settle),
            CLIPBOARD_GET => decode(body).map(Self::ClipboardGet),
            CLIPBOARD_SET => decode(body).map(Self::ClipboardSet),
            other => Err(Error::Tag(other)),
        }
    }
}

/// One desktop session in `info`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GuiSession {
    /// How a request names it.
    pub id: String,
    /// The account that owns it; only a peer holding `gui.view` or `gui.input` is told.
    #[serde(default)]
    pub user: Option<String>,
    /// Whether it is the session at the screen.
    #[serde(default)]
    pub active: bool,
    /// Whether the screen is locked.
    #[serde(default)]
    pub locked: Option<bool>,
    /// Its displays.
    #[serde(default)]
    pub displays: Vec<Display>,
}

/// What a host reports of its desktop in `info`; asked again at every `info`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiInfo {
    /// `[gui] enabled`.
    pub enabled: bool,
    /// A desktop can be used now.
    pub available: bool,
    /// Why not, when it cannot.
    pub reason: Option<String>,
    /// Which backend serves it.
    pub backend: Option<String>,
    /// Keyboard and mouse input are possible.
    pub input: bool,
    /// The clipboard's text is reachable.
    pub clipboard: bool,
    /// The host can tell a locked screen.
    pub lock_detection: bool,
    /// The image formats a snapshot may ask for.
    pub formats: Vec<ImageFormat>,
    /// The desktop sessions; empty unless the peer holds `gui.view` or `gui.input`.
    pub sessions: Vec<GuiSession>,
}

/// One approval a node waits for: an owner at the desk has not yet allowed or denied it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRequest {
    /// What `bohio gui allow` and `deny` name.
    pub id: u64,
    /// The peer's id.
    pub peer: String,
    /// The desktop session.
    pub session: String,
    /// What the peer asked for.
    pub want: Vec<Want>,
    /// When it began to wait, in seconds since the Unix epoch.
    pub since: u64,
}

/// What a `bohio-gui-approve/1` stream says: the waiting requests, first at once and then whenever they change, and the
/// answer to each [`ApproveInput::Decide`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApproveOutput {
    /// The requests waiting now.
    Pending {
        /// Oldest first.
        requests: Vec<PendingRequest>,
    },
    /// A decision was taken.
    Decided {
        /// The request.
        id: u64,
        /// The request was waiting; `false` when it had ended or never was.
        found: bool,
    },
}

impl ApproveOutput {
    /// Writes the message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when it cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a message.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not one.
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        decode(message)
    }
}

/// What a client of `bohio-gui-approve/1` sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApproveInput {
    /// Allows or denies a waiting request.
    Decide {
        /// The request.
        id: u64,
        /// `true` lets the stream open.
        allow: bool,
    },
}

impl ApproveInput {
    /// Writes the message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when it cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a message.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not one.
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        decode(message)
    }
}

/// A helper's first message, the open metadata of its `bohio-gui-helper/1` stream: which session it serves. The node
/// takes who the helper is from the connection (the operating system's answer), never from this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelperHello {
    /// The helper's own name for its session (a Windows session number, an X display, a login session); the node
    /// puts the verified account before it, so one account cannot name another's.
    pub session: String,
    /// The account's name for a person; informational.
    #[serde(default)]
    pub user: Option<String>,
    /// The session's state at the start.
    #[serde(default)]
    pub state: State,
}

impl HelperHello {
    /// Writes the hello as an open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when it cannot be written.
    pub fn to_metadata(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a hello from an open's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not one.
    pub fn from_metadata(metadata: &[u8]) -> Result<Self, Error> {
        decode(metadata)
    }
}

/// What the node tells a helper to apply on top of the user's own `[gui]` keys: each value is a bound the helper
/// narrows to and never widens (the stricter of the two wins).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperPolicy {
    /// The node's `[gui] indicator = "notify"`.
    pub indicator_notify: bool,
    /// The node's `[gui] input_when_locked`.
    pub input_when_locked: bool,
    /// The node's `[gui] max_input_rate`.
    pub max_input_rate: u32,
    /// The node's `[gui] max_snapshot_pixels`.
    pub max_snapshot_pixels: u64,
    /// The node's `[gui] max_batch_events`.
    pub max_batch_events: u64,
    /// The node's `[gui] max_text`.
    pub max_text: u64,
    /// The node's `[gui] clipboard_max_bytes`.
    pub clipboard_max_bytes: u64,
    /// The node's `[gui] max_fps`.
    pub max_fps: u32,
}

/// The open metadata of the `bohio-gui/1` stream a node opens on a helper for a peer's stream: the peer's own request
/// and what the node decided for it. The helper holds no key and reads no peer table, so this is all it knows of the
/// peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelperOpen {
    /// The peer's request, its `session` the one the node names the helper's session by.
    pub request: GuiRequest,
    /// The peer's id.
    pub peer: String,
    /// The session's name as the node lists it.
    pub session: String,
    /// What the node's grant for the peer allows of what the request asked: nothing else is served.
    pub granted: Vec<Want>,
    /// The node's own bounds.
    pub policy: HelperPolicy,
}

impl HelperOpen {
    /// Writes the open as metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when it cannot be written.
    pub fn to_metadata(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads an open from metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not one.
    pub fn from_metadata(metadata: &[u8]) -> Result<Self, Error> {
        decode(metadata)
    }
}

/// What a helper reports of a stream it served, for the node's audit: counts only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperChange {
    /// A stream opened with what it may do.
    Started {
        /// What the stream may do.
        granted: Vec<Want>,
    },
    /// A stream ended.
    Ended {
        /// Snapshots sent.
        snapshots: u64,
        /// Screen updates sent.
        frames: u64,
        /// Input events applied.
        input_events: u64,
        /// Clipboard reads.
        clipboard_reads: u64,
        /// Clipboard writes.
        clipboard_writes: u64,
        /// Why.
        reason: String,
    },
    /// The clipboard was read.
    ClipboardRead,
    /// The clipboard was written.
    ClipboardWritten,
    /// An open was refused.
    Refused {
        /// Why, with no request content.
        reason: String,
    },
}

/// What a helper sends on its `bohio-gui-helper/1` stream after the hello.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperMessage {
    /// The session's state changed (locked, switched away, a display changed).
    State {
        /// The state now.
        state: State,
    },
    /// A served stream's audit event.
    Event {
        /// The peer the node named in the stream's open.
        peer: String,
        /// What happened.
        change: HelperChange,
    },
}

impl HelperMessage {
    /// Writes the message.
    ///
    /// # Errors
    ///
    /// [`Error::Encode`] when it cannot be written.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    /// Reads a message.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the bytes are not one.
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        decode(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_a_listed_name_or_one_character() {
        for key in ["enter", "f24", "a", "é", "7", "page_down", "/"] {
            assert!(is_key(key), "{key}");
        }
        for key in ["", "Enter", "ab", "f25", "ctrl+s", "Return"] {
            assert!(!is_key(key), "{key}");
        }
    }

    #[test]
    fn a_combination_holds_its_modifiers_around_the_last_key() {
        let events = InputEvent::combo("ctrl+Shift+T").unwrap();
        let text = |event: &InputEvent| match event {
            InputEvent::Key { key, action } => format!("{key}:{action:?}"),
            _ => String::new(),
        };
        let seen: Vec<String> = events.iter().map(text).collect();
        assert_eq!(
            seen,
            [
                "control:Press",
                "shift:Press",
                "T:Click",
                "shift:Release",
                "control:Release"
            ]
        );
        assert_eq!(InputEvent::combo("Return").unwrap().len(), 1);
        assert_eq!(InputEvent::combo("super+Page_Down").unwrap().len(), 3);
        assert!(InputEvent::combo("ctrl+nokey").is_err());
        assert!(InputEvent::combo("").is_err());
    }

    #[test]
    fn a_request_without_want_asks_to_view() {
        let request = GuiRequest::from_metadata(&encode(&GuiRequest::default()).unwrap()).unwrap();
        assert_eq!(request.wants(), vec![Want::View]);
        assert_eq!(
            GuiRequest::new([Want::Input, Want::View, Want::Input]).wants(),
            vec![Want::View, Want::Input]
        );
    }

    #[test]
    fn frames_round_trip_and_an_unknown_tag_is_refused() {
        let input = GuiInput::Input(Input {
            seq: 4,
            events: vec![
                InputEvent::Move {
                    display: None,
                    x: 3,
                    y: -2,
                },
                InputEvent::Text { text: "hi".into() },
            ],
        });
        assert_eq!(GuiInput::decode(&input.encode().unwrap()).unwrap(), input);
        let data = GuiOutput::Data(vec![1, 2, 3]);
        assert_eq!(GuiOutput::decode(&data.encode().unwrap()).unwrap(), data);
        assert_eq!(GuiOutput::decode(&[0x7f]), Err(Error::Tag(0x7f)));
        assert_eq!(GuiInput::decode(&[]), Err(Error::Short));
    }
}
