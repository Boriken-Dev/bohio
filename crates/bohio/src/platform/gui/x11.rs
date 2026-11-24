//! The Linux desktop over X11: the screen of an X display, as a [`Desktop`].
//!
//! The protocol is spoken by `x11rb`'s pure-Rust connection (no libxcb, so a static musl build keeps it). Displays are
//! RandR's monitors (the root window's whole area when the server has none), a capture is `GetImage` of the root window
//! converted from the visual's masks, and input is XTest `FakeInput` on the root. A character the keyboard has no key
//! for is typed through a spare keycode bound to it for the moment. The lock state is logind's `LockedHint`
//! ([`super::logind`]); the clipboard is a selection this node owns ([`super::x11_clipboard`]).
//!
//! The rules that need no server (the layout arithmetic, the pixel formats, the keyboard plan) are plain functions
//! tested without one.

#![forbid(unsafe_code)]

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use bohio_proto::ErrorCode;
use bohio_proto::gui::{Button, ButtonAction, Display, DisplayId, InputEvent, KeyAction, Rect};
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::ErrorKind;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, ImageFormat, ImageOrder,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, MapState, VisualClass, Window,
    WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use xkeysym::{Keysym, key};

use super::logind::Logind;
use super::x11_clipboard::Selection;
use crate::gui::{Capabilities, Desktop, DesktopState, Frame, GuiError};

/// What a node says of a Wayland session when it is asked for the X display.
pub(crate) const WAYLAND_SESSION: &str = "this is a Wayland session: an X11 connection reaches only the X11 windows in it. \
     This build has no Wayland backend (the gui-wayland feature); serve the X11 windows with x11_in_wayland = true in \
     [source.native]";

/// How the X11 backend runs; the `native` source builds it from `[source.native]` and the command's environment.
#[derive(Debug, Clone)]
pub struct X11Settings {
    /// The display to serve (`:0`); `None` is a node with no display, which reports itself unavailable.
    pub display: Option<String>,
    /// Whether the session is a Wayland one (`WAYLAND_DISPLAY` set, or `XDG_SESSION_TYPE` is `wayland`): its X display
    /// is XWayland's, which holds only the X11 windows.
    pub wayland: bool,
    /// Serve a Wayland session's X display all the same (its X11 windows only).
    pub in_wayland: bool,
    /// The login session to ask logind about (`XDG_SESSION_ID`); `None` asks for this process's own.
    pub session: Option<String>,
    /// The address of the system bus, when it is not the standard one.
    pub system_bus: Option<String>,
    /// How long logind may take to answer.
    pub bus_wait: Duration,
    /// How long the keyboard mapping is left changed around a character typed through a spare keycode, so that the
    /// programs that follow a mapping change see it.
    pub key_wait: Duration,
    /// How long the clipboard waits for the program that owns it.
    pub clipboard_wait: Duration,
    /// The most bytes of clipboard text read or written.
    pub clipboard_max_bytes: usize,
    /// The selection that is the clipboard: `CLIPBOARD`, or `PRIMARY`, or any other name.
    pub selection: String,
    /// A desktop of the node's own that no person sits at: it has no lock screen, so it is never locked and does not ask
    /// logind.
    pub headless: bool,
    /// Why this node cannot serve its display at all (the own session's runtime directory is refused): every call answers
    /// `unavailable` with it.
    pub refused: Option<String>,
    /// What to add to the refusal of a display that does not answer: where an own desktop's units are looked at.
    pub hint: Option<String>,
    /// The most clicks of one button event and wheel notches of one scroll event; more is `bad_request`.
    pub max_repeat: u32,
}

impl Default for X11Settings {
    fn default() -> Self {
        Self {
            display: None,
            wayland: false,
            in_wayland: false,
            session: None,
            system_bus: None,
            bus_wait: Duration::from_secs(1),
            key_wait: Duration::from_millis(20),
            clipboard_wait: Duration::from_millis(500),
            clipboard_max_bytes: 1024 * 1024,
            headless: false,
            refused: None,
            hint: None,
            selection: "CLIPBOARD".to_owned(),
            max_repeat: 1000,
        }
    }
}

fn failed(what: &str, error: impl std::fmt::Display) -> GuiError {
    GuiError::io(format!("{what}: {error}"))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ----------------------------------------------------------------------------------------------------------------
// Layout: displays and root coordinates.

/// The id a monitor named `name` has: its name lowercased with every other character a dash, else `display` and its
/// place; a repeated one gets its place after it.
pub(crate) fn display_id(name: &str, index: usize, taken: &[String]) -> String {
    let mut id: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if id.trim_matches('-').is_empty() {
        id = format!("display{}", index + 1);
    }
    if taken.contains(&id) {
        id = format!("{id}-{}", index + 1);
    }
    id
}

/// Exactly one display is primary: the first marked, else the first.
pub(crate) fn one_primary(displays: &mut [Display]) {
    let keep = displays.iter().position(|d| d.primary).unwrap_or(0);
    for (index, display) in displays.iter_mut().enumerate() {
        display.primary = index == keep;
    }
}

/// A rectangle of the root window: its left and top edges (which RandR places a monitor on), and its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RootRect {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// The part of the root window that `region` of `display` is: a monitor's pixels start at its place in the root.
///
/// # Errors
///
/// `bad_request` for a region off its display or beyond the protocol's 16-bit coordinates.
pub(crate) fn root_rect(display: &Display, region: Rect) -> Result<RootRect, GuiError> {
    if !region.fits(display.width, display.height) {
        return Err(GuiError::bad_request("the region lies outside the display"));
    }
    let off = || {
        GuiError::bad_request("the region lies beyond what the display's root window can address")
    };
    Ok(RootRect {
        x: i16::try_from(i64::from(display.x) + i64::from(region.x)).map_err(|_| off())?,
        y: i16::try_from(i64::from(display.y) + i64::from(region.y)).map_err(|_| off())?,
        width: u16::try_from(region.width).map_err(|_| off())?,
        height: u16::try_from(region.height).map_err(|_| off())?,
    })
}

/// The root point of the pixel `x`, `y` of `display`.
///
/// # Errors
///
/// `bad_request` for a point off its display.
pub(crate) fn root_point(display: &Display, x: i32, y: i32) -> Result<(i16, i16), GuiError> {
    let inside = x >= 0
        && y >= 0
        && i64::from(x) < i64::from(display.width)
        && i64::from(y) < i64::from(display.height);
    if !inside {
        return Err(GuiError::bad_request(format!(
            "the point lies outside the display ({} by {} pixels)",
            display.width, display.height
        )));
    }
    let off = || {
        GuiError::bad_request("the point lies beyond what the display's root window can address")
    };
    Ok((
        i16::try_from(i64::from(display.x) + i64::from(x)).map_err(|_| off())?,
        i16::try_from(i64::from(display.y) + i64::from(y)).map_err(|_| off())?,
    ))
}

// ----------------------------------------------------------------------------------------------------------------
// Pixels.

/// How the server lays out the pixels of the root's depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelFormat {
    pub bits_per_pixel: u8,
    pub scanline_pad: u8,
    pub little_endian: bool,
    pub red: u32,
    pub green: u32,
    pub blue: u32,
}

impl PixelFormat {
    /// Bytes of one row of `width` pixels.
    fn stride(&self, width: usize) -> usize {
        let pad = usize::from(self.scanline_pad.max(8));
        (width * usize::from(self.bits_per_pixel)).div_ceil(pad) * pad / 8
    }
}

/// One colour channel: the bits a mask holds, scaled to 8.
struct Channel {
    mask: u32,
    shift: u32,
    max: u32,
}

impl Channel {
    fn new(mask: u32) -> Self {
        let shift = mask.trailing_zeros().min(31);
        Self {
            mask,
            shift,
            max: mask >> shift,
        }
    }

    fn of(&self, pixel: u32) -> u8 {
        if self.max == 0 {
            return 0;
        }
        let value = (pixel & self.mask) >> self.shift;
        if self.max == 255 {
            return u8::try_from(value).unwrap_or(255);
        }
        u8::try_from((u64::from(value) * 255 + u64::from(self.max) / 2) / u64::from(self.max))
            .unwrap_or(255)
    }
}

/// The RGB bytes of `width` by `height` pixels the server sent in `format`.
///
/// # Errors
///
/// `unsupported` for a pixel size other than 16, 24 or 32 bits; `io` for data shorter than the size says.
pub(crate) fn rgb_of(
    data: &[u8],
    width: usize,
    height: usize,
    format: &PixelFormat,
) -> Result<Vec<u8>, GuiError> {
    let bytes = match format.bits_per_pixel {
        16 => 2,
        24 => 3,
        32 => 4,
        other => {
            return Err(GuiError::unsupported(format!(
                "the display uses {other} bits a pixel, which this host cannot read"
            )));
        }
    };
    let stride = format.stride(width);
    if data.len() < stride * height || stride < width * bytes {
        return Err(GuiError::io(
            "the display sent a picture shorter than its size",
        ));
    }
    let (red, green, blue) = (
        Channel::new(format.red),
        Channel::new(format.green),
        Channel::new(format.blue),
    );
    let mut rgb = Vec::with_capacity(width * height * 3);
    for row in data.chunks(stride).take(height) {
        for pixel in row.chunks(bytes).take(width) {
            let mut value = 0_u32;
            if format.little_endian {
                for (index, byte) in pixel.iter().enumerate() {
                    value |= u32::from(*byte) << (8 * index);
                }
            } else {
                for byte in pixel {
                    value = (value << 8) | u32::from(*byte);
                }
            }
            rgb.extend_from_slice(&[red.of(value), green.of(value), blue.of(value)]);
        }
    }
    Ok(rgb)
}

/// Why a picture of a drawable was not made.
enum Grab {
    /// The server will not give a picture of this drawable (`BadMatch`): the root of a display that holds no screen pixels.
    Refused,
    Failed(GuiError),
}

/// The part of the window `x`, `y`, `width` by `height` (in root coordinates) that lies inside `at`: its root position
/// and size, or `None` when they do not meet.
pub(crate) fn overlap(
    at: RootRect,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Option<(i32, i32, u32, u32)> {
    let left = x.max(i32::from(at.x));
    let top = y.max(i32::from(at.y));
    let right = i64::from(x)
        .saturating_add(i64::from(width))
        .min(i64::from(at.x) + i64::from(at.width));
    let bottom = i64::from(y)
        .saturating_add(i64::from(height))
        .min(i64::from(at.y) + i64::from(at.height));
    if i64::from(left) >= right || i64::from(top) >= bottom {
        return None;
    }
    Some((
        left,
        top,
        u32::try_from(right - i64::from(left)).ok()?,
        u32::try_from(bottom - i64::from(top)).ok()?,
    ))
}

/// Copies `piece` (`piece_width` pixels a row) into `canvas` (`canvas_width` pixels a row) at `at`, a column and a row.
pub(crate) fn paste(
    canvas: &mut [u8],
    canvas_width: usize,
    at: (usize, usize),
    piece: &[u8],
    piece_width: usize,
) {
    if piece_width == 0 {
        return;
    }
    for (row, line) in piece.chunks(piece_width * 3).enumerate() {
        let start = ((at.1 + row) * canvas_width + at.0) * 3;
        if let Some(target) = canvas.get_mut(start..start + line.len()) {
            target.copy_from_slice(line);
        }
    }
}

// ----------------------------------------------------------------------------------------------------------------
// The keyboard.

/// The server's keyboard mapping: `per` keysyms for each keycode from `first`.
#[derive(Debug, Clone)]
pub(crate) struct KeyMap {
    first: u8,
    per: usize,
    syms: Vec<u32>,
}

impl KeyMap {
    pub(crate) fn new(first: u8, per: usize, syms: Vec<u32>) -> Self {
        Self {
            first,
            per: per.max(1),
            syms,
        }
    }

    fn rows(&self) -> impl Iterator<Item = (u8, &[u32])> {
        self.syms
            .chunks(self.per)
            .enumerate()
            .filter_map(|(index, row)| {
                Some((self.first.checked_add(u8::try_from(index).ok()?)?, row))
            })
    }

    /// The keycode that makes `sym` as its first symbol, or as its second (with shift held), the first preferred.
    pub(crate) fn find(&self, sym: u32) -> Option<(u8, bool)> {
        for column in 0..2 {
            for (code, row) in self.rows() {
                if row.get(column) == Some(&sym) {
                    return Some((code, column == 1));
                }
            }
        }
        None
    }

    /// A keycode that has no symbol at all, the highest.
    pub(crate) fn spare(&self) -> Option<u8> {
        self.rows()
            .filter(|(_, row)| row.iter().all(|sym| *sym == 0))
            .map(|(code, _)| code)
            .last()
    }
}

/// How one key is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stroke {
    /// A key the keyboard has, with the code of shift when the symbol is its second.
    Direct { code: u8, shift: Option<u8> },
    /// A symbol the keyboard has no key for: a spare keycode is bound to it.
    Spare { code: u8, sym: u32, per: usize },
}

impl Stroke {
    fn of(map: &KeyMap, sym: u32) -> Result<Self, GuiError> {
        if let Some((code, shifted)) = map.find(sym) {
            if !shifted {
                return Ok(Self::Direct { code, shift: None });
            }
            if let Some((shift, false)) = map.find(key::Shift_L) {
                return Ok(Self::Direct {
                    code,
                    shift: Some(shift),
                });
            }
        }
        let code = map.spare().ok_or_else(|| {
            GuiError::unavailable("the keyboard has no spare key to type this with")
        })?;
        Ok(Self::Spare {
            code,
            sym,
            per: map.per,
        })
    }
}

/// What one step of an event does on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Motion {
        x: i16,
        y: i16,
    },
    Button {
        detail: u8,
        down: bool,
    },
    Key {
        code: u8,
        down: bool,
    },
    /// Binds `sym` to the keycode.
    Remap {
        code: u8,
        sym: u32,
        per: usize,
    },
    /// Takes the binding away again.
    Unmap {
        code: u8,
        per: usize,
    },
    /// Waits `key_wait` for the programs to follow the keyboard mapping.
    Pause,
}

fn key_steps(stroke: Stroke, action: KeyAction) -> Vec<Step> {
    let down = action != KeyAction::Release;
    let up = action != KeyAction::Press;
    let mut steps = Vec::new();
    match stroke {
        Stroke::Direct { code, shift } => {
            if down {
                if let Some(shift) = shift {
                    steps.push(Step::Key {
                        code: shift,
                        down: true,
                    });
                }
                steps.push(Step::Key { code, down: true });
            }
            if up {
                steps.push(Step::Key { code, down: false });
                if let Some(shift) = shift {
                    steps.push(Step::Key {
                        code: shift,
                        down: false,
                    });
                }
            }
        }
        Stroke::Spare { code, sym, per } => {
            steps.push(Step::Remap { code, sym, per });
            steps.push(Step::Pause);
            if down {
                steps.push(Step::Key { code, down: true });
            }
            if up {
                steps.push(Step::Key { code, down: false });
                steps.push(Step::Pause);
                steps.push(Step::Unmap { code, per });
            }
        }
    }
    steps
}

/// The keysym of a key a request names: a name of the closed list, or one character.
pub(crate) fn keysym_of(name: &str) -> Option<u32> {
    let mut characters = name.chars();
    if let (Some(first), None) = (characters.next(), characters.next()) {
        if first.is_ascii_alphabetic() {
            return Some(u32::from(first.to_ascii_lowercase()));
        }
        let sym = Keysym::from_char(first);
        return (sym != Keysym::NoSymbol).then(|| sym.raw());
    }
    Some(match name {
        "enter" => key::Return,
        "tab" => key::Tab,
        "escape" => key::Escape,
        "backspace" => key::BackSpace,
        "delete" => key::Delete,
        "insert" => key::Insert,
        "home" => key::Home,
        "end" => key::End,
        "page_up" => key::Prior,
        "page_down" => key::Next,
        "up" => key::Up,
        "down" => key::Down,
        "left" => key::Left,
        "right" => key::Right,
        "space" => key::space,
        "shift" => key::Shift_L,
        "control" => key::Control_L,
        "alt" => key::Alt_L,
        "meta" => key::Super_L,
        "caps_lock" => key::Caps_Lock,
        "print_screen" => key::Print,
        "menu" => key::Menu,
        _ => {
            let number: u32 = name.strip_prefix('f')?.parse().ok()?;
            if !(1..=24).contains(&number) {
                return None;
            }
            key::F1 + number - 1
        }
    })
}

/// The steps that type `text` as it is: a character as itself, a line break as Return and a tab as Tab.
pub(crate) fn text_steps(text: &str, map: &KeyMap) -> Result<Vec<Step>, GuiError> {
    let mut steps = Vec::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        let sym = match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                key::Return
            }
            '\n' => key::Return,
            '\t' => key::Tab,
            other => {
                let sym = Keysym::from_char(other);
                if sym == Keysym::NoSymbol {
                    return Err(GuiError::bad_request("a character that has no keysym"));
                }
                sym.raw()
            }
        };
        steps.extend(key_steps(Stroke::of(map, sym)?, KeyAction::Click));
    }
    Ok(steps)
}

fn button_detail(button: Button) -> u8 {
    match button {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
        Button::Back => 8,
        Button::Forward => 9,
    }
}

/// The steps of one event, in order. `wait` has none (the engine pauses itself).
///
/// # Errors
///
/// `bad_request` for a point off its display, a key that is not one, or a repeat past `max_repeat`.
pub(crate) fn plan(
    event: &InputEvent,
    state: &DesktopState,
    map: &KeyMap,
    max_repeat: u32,
) -> Result<Vec<Step>, GuiError> {
    let motion = |display: Option<&DisplayId>, x: i32, y: i32| -> Result<Step, GuiError> {
        let (x, y) = root_point(state.display(display)?, x, y)?;
        Ok(Step::Motion { x, y })
    };
    let repeat = |count: u32| -> Result<u32, GuiError> {
        if count > max_repeat {
            return Err(GuiError::bad_request(format!(
                "that repeats {count} times; this host repeats at most {max_repeat}"
            )));
        }
        Ok(count)
    };
    Ok(match event {
        InputEvent::Move { display, x, y } => vec![motion(display.as_ref(), *x, *y)?],
        InputEvent::Button {
            button,
            action,
            count,
            display,
            x,
            y,
        } => {
            let mut steps = Vec::new();
            if let (Some(x), Some(y)) = (x, y) {
                steps.push(motion(display.as_ref(), *x, *y)?);
            }
            let detail = button_detail(*button);
            match action {
                ButtonAction::Press => steps.push(Step::Button { detail, down: true }),
                ButtonAction::Release => steps.push(Step::Button {
                    detail,
                    down: false,
                }),
                ButtonAction::Click => {
                    for _ in 0..repeat(count.unwrap_or(1).max(1))? {
                        steps.push(Step::Button { detail, down: true });
                        steps.push(Step::Button {
                            detail,
                            down: false,
                        });
                    }
                }
            }
            steps
        }
        InputEvent::Scroll {
            display,
            x,
            y,
            dx,
            dy,
        } => {
            let mut steps = Vec::new();
            if let (Some(x), Some(y)) = (x, y) {
                steps.push(motion(display.as_ref(), *x, *y)?);
            }
            // The core protocol's wheel: 4 up, 5 down, 6 left, 7 right. The wire's positive `dy` scrolls down.
            for (notches, positive, negative) in [(*dy, 5_u8, 4_u8), (*dx, 7, 6)] {
                let detail = if notches > 0 { positive } else { negative };
                for _ in 0..repeat(notches.unsigned_abs())? {
                    steps.push(Step::Button { detail, down: true });
                    steps.push(Step::Button {
                        detail,
                        down: false,
                    });
                }
            }
            steps
        }
        InputEvent::Key { key, action } => {
            let sym =
                keysym_of(key).ok_or_else(|| GuiError::bad_request("that is not a key name"))?;
            key_steps(Stroke::of(map, sym)?, *action)
        }
        InputEvent::Text { text } => text_steps(text, map)?,
        InputEvent::Wait { .. } => Vec::new(),
    })
}

// ----------------------------------------------------------------------------------------------------------------
// The connection.

/// A connection to the display and what was asked of it once.
struct Link {
    connection: RustConnection,
    root: Window,
    input: bool,
    monitors: bool,
    keys: (u8, u8),
    display: String,
}

impl Link {
    fn connect(settings: &X11Settings) -> Result<Self, GuiError> {
        let Some(display) = settings.display.as_deref() else {
            return Err(GuiError::unavailable(
                "this node has no X display: it was started without one",
            ));
        };
        let (connection, number) = RustConnection::connect(Some(display)).map_err(|error| {
            GuiError::unavailable(format!(
                "cannot connect to the X display {display}: {error}{}",
                settings
                    .hint
                    .as_deref()
                    .map(|hint| format!(" ({hint})"))
                    .unwrap_or_default()
            ))
        })?;
        let setup = connection.setup();
        let screen = setup
            .roots
            .get(number)
            .ok_or_else(|| GuiError::unavailable("the X display has no such screen"))?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| &depth.visuals)
            .find(|visual| visual.visual_id == screen.root_visual)
            .ok_or_else(|| GuiError::unavailable("the root window's visual is not listed"))?;
        if visual.class != VisualClass::TRUE_COLOR && visual.class != VisualClass::DIRECT_COLOR {
            return Err(GuiError::unsupported(
                "the display's visual is not true colour, which this host cannot read",
            ));
        }
        let keys = (setup.min_keycode, setup.max_keycode);
        let root = screen.root;
        let input = connection
            .xtest_get_version(2, 2)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some();
        let monitors = connection
            .randr_query_version(1, 5)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|version| (version.major_version, version.minor_version) >= (1, 5));
        Ok(Self {
            connection,
            root,
            input,
            monitors,
            keys,
            display: display.to_owned(),
        })
    }

    /// How a reply of `depth` and `visual` lays out its pixels.
    fn format_of(&self, depth: u8, visual: u32) -> Result<PixelFormat, GuiError> {
        let setup = self.connection.setup();
        let pixmap = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .ok_or_else(|| {
                GuiError::unavailable("the display lists no pixel format for a depth")
            })?;
        let visual = setup
            .roots
            .iter()
            .flat_map(|screen| &screen.allowed_depths)
            .flat_map(|depth| &depth.visuals)
            .find(|candidate| candidate.visual_id == visual)
            .ok_or_else(|| {
                GuiError::unavailable("a picture came in a visual the display does not list")
            })?;
        if visual.class != VisualClass::TRUE_COLOR && visual.class != VisualClass::DIRECT_COLOR {
            return Err(GuiError::unsupported(
                "the display's visual is not true colour, which this host cannot read",
            ));
        }
        Ok(PixelFormat {
            bits_per_pixel: pixmap.bits_per_pixel,
            scanline_pad: pixmap.scanline_pad,
            little_endian: setup.image_byte_order == ImageOrder::LSB_FIRST,
            red: visual.red_mask,
            green: visual.green_mask,
            blue: visual.blue_mask,
        })
    }

    /// The RGB pixels of a rectangle of `drawable`.
    fn read(&self, drawable: Window, at: RootRect) -> Result<Vec<u8>, Grab> {
        let reply = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                drawable,
                at.x,
                at.y,
                at.width,
                at.height,
                !0,
            )
            .map_err(|error| Grab::Failed(failed("capturing the screen", error)))?
            .reply()
            .map_err(|error| match error {
                ReplyError::X11Error(ref x11) if x11.error_kind == ErrorKind::Match => {
                    Grab::Refused
                }
                other => Grab::Failed(failed("capturing the screen", other)),
            })?;
        let format = self
            .format_of(reply.depth, reply.visual)
            .map_err(Grab::Failed)?;
        rgb_of(
            &reply.data,
            usize::from(at.width),
            usize::from(at.height),
            &format,
        )
        .map_err(Grab::Failed)
    }

    /// The pixels of the root rectangle `at` made from the windows that are on the root, bottom to top, over black.
    ///
    /// A root that cannot be read (the X display of a Wayland session holds no screen pixels) still has windows that can,
    /// so the picture is the X11 windows in it and nothing else.
    fn windows_rgb(&self, at: RootRect) -> Result<Vec<u8>, GuiError> {
        let (width, height) = (usize::from(at.width), usize::from(at.height));
        let mut canvas = vec![0_u8; width * height * 3];
        let children = self
            .connection
            .query_tree(self.root)
            .map_err(|error| failed("listing the windows", error))?
            .reply()
            .map_err(|error| failed("listing the windows", error))?
            .children;
        for child in children {
            let Some((attributes, geometry)) = self
                .connection
                .get_window_attributes(child)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .zip(
                    self.connection
                        .get_geometry(child)
                        .ok()
                        .and_then(|cookie| cookie.reply().ok()),
                )
            else {
                continue;
            };
            if attributes.class != WindowClass::INPUT_OUTPUT
                || attributes.map_state != MapState::VIEWABLE
            {
                continue;
            }
            let Some(over) = overlap(
                at,
                i32::from(geometry.x),
                i32::from(geometry.y),
                u32::from(geometry.width),
                u32::from(geometry.height),
            ) else {
                continue;
            };
            let piece = RootRect {
                x: i16::try_from(over.0 - i32::from(geometry.x)).unwrap_or(0),
                y: i16::try_from(over.1 - i32::from(geometry.y)).unwrap_or(0),
                width: u16::try_from(over.2).unwrap_or(0),
                height: u16::try_from(over.3).unwrap_or(0),
            };
            // A window may be gone by now, or refuse: it is left out.
            let Ok(rgb) = self.read(child, piece) else {
                continue;
            };
            paste(
                &mut canvas,
                width,
                (
                    usize::try_from(over.0 - i32::from(at.x)).unwrap_or(0),
                    usize::try_from(over.1 - i32::from(at.y)).unwrap_or(0),
                ),
                &rgb,
                usize::from(piece.width),
            );
        }
        Ok(canvas)
    }

    fn root_size(&self) -> Result<(u32, u32), GuiError> {
        let geometry = self
            .connection
            .get_geometry(self.root)
            .map_err(|error| failed("asking the display for its size", error))?
            .reply()
            .map_err(|error| failed("asking the display for its size", error))?;
        Ok((u32::from(geometry.width), u32::from(geometry.height)))
    }

    fn displays(&self) -> Result<Vec<Display>, GuiError> {
        let mut displays: Vec<Display> = Vec::new();
        if self.monitors {
            let reply = self
                .connection
                .randr_get_monitors(self.root, true)
                .map_err(|error| failed("listing the monitors", error))?
                .reply()
                .map_err(|error| failed("listing the monitors", error))?;
            let mut taken: Vec<String> = Vec::new();
            for (index, monitor) in reply.monitors.iter().enumerate() {
                if monitor.width == 0 || monitor.height == 0 {
                    continue;
                }
                let name = self
                    .connection
                    .get_atom_name(monitor.name)
                    .ok()
                    .and_then(|cookie| cookie.reply().ok())
                    .map(|reply| String::from_utf8_lossy(&reply.name).into_owned());
                let id = display_id(name.as_deref().unwrap_or(""), index, &taken);
                taken.push(id.clone());
                displays.push(Display {
                    id: DisplayId(id),
                    name,
                    primary: monitor.primary,
                    x: i32::from(monitor.x),
                    y: i32::from(monitor.y),
                    width: u32::from(monitor.width),
                    height: u32::from(monitor.height),
                    scale: 1.0,
                    rotation: None,
                });
            }
        }
        if displays.is_empty() {
            let (width, height) = self.root_size()?;
            displays.push(Display {
                id: DisplayId("display1".to_owned()),
                name: None,
                primary: true,
                x: 0,
                y: 0,
                width,
                height,
                scale: 1.0,
                rotation: None,
            });
        }
        one_primary(&mut displays);
        Ok(displays)
    }

    fn keymap(&self) -> Result<KeyMap, GuiError> {
        let (first, last) = self.keys;
        let count = last - first + 1;
        let reply = self
            .connection
            .get_keyboard_mapping(first, count)
            .map_err(|error| failed("reading the keyboard mapping", error))?
            .reply()
            .map_err(|error| failed("reading the keyboard mapping", error))?;
        Ok(KeyMap::new(
            first,
            usize::from(reply.keysyms_per_keycode),
            reply.keysyms,
        ))
    }

    fn apply(&self, steps: &[Step], key_wait: Duration) -> Result<(), GuiError> {
        let connection = &self.connection;
        let fake = |kind: u8, detail: u8, x: i16, y: i16| {
            connection
                .xtest_fake_input(kind, detail, 0, self.root, x, y, 0)
                .map(drop)
                .map_err(|error| failed("sending input", error))
        };
        for step in steps {
            match *step {
                Step::Motion { x, y } => fake(MOTION_NOTIFY_EVENT, 0, x, y)?,
                Step::Button { detail, down } => fake(
                    if down {
                        BUTTON_PRESS_EVENT
                    } else {
                        BUTTON_RELEASE_EVENT
                    },
                    detail,
                    0,
                    0,
                )?,
                Step::Key { code, down } => fake(
                    if down {
                        KEY_PRESS_EVENT
                    } else {
                        KEY_RELEASE_EVENT
                    },
                    code,
                    0,
                    0,
                )?,
                Step::Remap { code, sym, per } => {
                    let syms = vec![sym; per];
                    connection
                        .change_keyboard_mapping(1, code, u8::try_from(per).unwrap_or(1), &syms)
                        .map_err(|error| failed("binding a key", error))?;
                }
                Step::Unmap { code, per } => {
                    let syms = vec![0_u32; per];
                    connection
                        .change_keyboard_mapping(1, code, u8::try_from(per).unwrap_or(1), &syms)
                        .map_err(|error| failed("unbinding a key", error))?;
                }
                Step::Pause => {
                    connection
                        .flush()
                        .map_err(|error| failed("sending input", error))?;
                    thread::sleep(key_wait);
                }
            }
        }
        // A request's error comes back as an event, not as a failure of the call: ask for something so that the server has
        // taken everything before this returns.
        connection
            .get_input_focus()
            .map_err(|error| failed("sending input", error))?
            .reply()
            .map_err(|error| failed("sending input", error))?;
        Ok(())
    }
}

/// The screen of an X display, served as a [`Desktop`].
///
/// The connection is made at the first call and made again after one that failed on the wire. A display with no XTest
/// extension serves views and no input; one with no RandR 1.5 is one display the size of its root window.
pub struct X11Desktop {
    settings: X11Settings,
    link: Mutex<Option<Link>>,
    logind: Logind,
    selection: Selection,
}

impl X11Desktop {
    /// A desktop run by `settings`. Nothing is connected until it is asked something.
    #[must_use]
    pub fn new(settings: X11Settings) -> Self {
        let logind = Logind::new(
            settings.system_bus.clone(),
            settings.session.clone(),
            settings.bus_wait,
        );
        Self {
            settings,
            link: Mutex::new(None),
            logind,
            selection: Selection::default(),
        }
    }

    /// Runs `f` on the connection, made first if there is none. A failure on the wire drops the connection.
    fn with_link<R>(&self, f: impl FnOnce(&Link) -> Result<R, GuiError>) -> Result<R, GuiError> {
        if let Some(reason) = &self.settings.refused {
            return Err(GuiError::unavailable(reason.clone()));
        }
        if self.settings.wayland && !self.settings.in_wayland {
            return Err(GuiError::unavailable(WAYLAND_SESSION));
        }
        let mut held = lock(&self.link);
        if held.is_none() {
            *held = Some(Link::connect(&self.settings)?);
        }
        let result = match held.as_ref() {
            Some(link) => f(link),
            None => Err(GuiError::unavailable("the X display is not connected")),
        };
        if result
            .as_ref()
            .is_err_and(|error| error.code == ErrorCode::Io)
        {
            *held = None;
        }
        result
    }

    fn state_of(&self, link: &Link) -> Result<DesktopState, GuiError> {
        Ok(DesktopState {
            user: None,
            // The seat's notion of an active session does not apply to a display that a remote login or a nested server
            // serves: a display that answers is at the screen.
            active: true,
            locked: if self.settings.headless {
                Some(false)
            } else {
                self.logind.hints().map(|hints| hints.locked)
            },
            displays: link.displays()?,
        })
    }
}

impl Desktop for X11Desktop {
    fn state(&self) -> Result<DesktopState, GuiError> {
        self.with_link(|link| self.state_of(link))
    }

    fn capture(&self, display: &DisplayId, region: Option<Rect>) -> Result<Frame, GuiError> {
        self.with_link(|link| {
            let displays = link.displays()?;
            let found = displays
                .iter()
                .find(|candidate| &candidate.id == display)
                .ok_or_else(|| GuiError::not_found("no such display"))?;
            let region = region.unwrap_or_else(|| Rect::new(0, 0, found.width, found.height));
            let at = root_rect(found, region)?;
            let rgb = match link.read(link.root, at) {
                Ok(rgb) => rgb,
                Err(Grab::Refused) => link.windows_rgb(at)?,
                Err(Grab::Failed(error)) => return Err(error),
            };
            Frame::new(region.width, region.height, rgb)
        })
    }

    fn input(&self, events: &[InputEvent]) -> Result<u32, GuiError> {
        self.with_link(|link| {
            if !link.input {
                return Err(GuiError::unsupported(
                    "the display has no XTEST extension, so it takes no input",
                ));
            }
            let state = self.state_of(link)?;
            if state.locked == Some(true) {
                return Err(GuiError::unavailable(
                    "the session is locked: input is not sent to a lock screen",
                ));
            }
            let map = link.keymap()?;
            // Every event is planned before any is sent, so one that names a point off its display refuses the batch whole.
            let planned: Vec<Vec<Step>> = events
                .iter()
                .map(|event| plan(event, &state, &map, self.settings.max_repeat))
                .collect::<Result<_, _>>()?;
            let mut applied = 0_u32;
            for steps in planned {
                if let Err(error) = link.apply(&steps, self.settings.key_wait) {
                    return if applied == 0 {
                        Err(error)
                    } else {
                        Ok(applied)
                    };
                }
                applied += 1;
            }
            Ok(applied)
        })
    }

    fn cursor(&self) -> Option<(DisplayId, i32, i32)> {
        self.with_link(|link| {
            let pointer = link
                .connection
                .query_pointer(link.root)
                .map_err(|error| failed("asking for the pointer", error))?
                .reply()
                .map_err(|error| failed("asking for the pointer", error))?;
            let (x, y) = (i32::from(pointer.root_x), i32::from(pointer.root_y));
            Ok(link.displays()?.into_iter().find_map(|display| {
                let inside = x >= display.x
                    && y >= display.y
                    && i64::from(x) < i64::from(display.x) + i64::from(display.width)
                    && i64::from(y) < i64::from(display.y) + i64::from(display.height);
                inside.then(|| (display.id, x - display.x, y - display.y))
            }))
        })
        .ok()
        .flatten()
    }

    fn clipboard_text(&self) -> Result<String, GuiError> {
        self.with_link(|link| self.selection.read(&link.display, &self.settings))
    }

    fn set_clipboard_text(&self, text: &str) -> Result<(), GuiError> {
        self.with_link(|link| self.selection.write(&link.display, &self.settings, text))
    }

    fn capabilities(&self) -> Capabilities {
        let input = lock(&self.link).as_ref().is_none_or(|link| link.input);
        Capabilities {
            backend: "x11".to_owned(),
            input,
            clipboard: true,
            lock_detection: !self.settings.headless && self.logind.hints().is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: &str, primary: bool, x: i32, y: i32, width: u32, height: u32) -> Display {
        Display {
            id: DisplayId(id.to_owned()),
            name: None,
            primary,
            x,
            y,
            width,
            height,
            scale: 1.0,
            rotation: None,
        }
    }

    fn two() -> DesktopState {
        DesktopState {
            user: None,
            active: true,
            locked: Some(false),
            displays: vec![
                display("left", true, 0, 0, 1280, 800),
                display("right", false, 1280, 0, 1024, 768),
            ],
        }
    }

    /// A US keyboard in miniature: a, b and 1 with their shifted symbols, Return and Shift_L, and room to spare.
    fn map() -> KeyMap {
        let mut syms = vec![0; 2 * 20];
        let set = |syms: &mut Vec<u32>, code: usize, first: u32, second: u32| {
            syms[(code - 8) * 2] = first;
            syms[(code - 8) * 2 + 1] = second;
        };
        set(&mut syms, 10, u32::from(b'1'), u32::from(b'!'));
        set(&mut syms, 11, u32::from(b'a'), u32::from(b'A'));
        set(&mut syms, 12, u32::from(b'b'), u32::from(b'B'));
        set(&mut syms, 13, key::Return, 0);
        set(&mut syms, 14, key::Shift_L, 0);
        KeyMap::new(8, 2, syms)
    }

    #[test]
    fn a_monitor_pixel_is_the_root_pixel_at_the_monitors_place() {
        let state = two();
        let right = &state.displays[1];
        assert_eq!(root_point(right, 10, 20).unwrap(), (1290, 20));
        let rect = root_rect(right, Rect::new(5, 6, 100, 50)).unwrap();
        assert_eq!(
            rect,
            RootRect {
                x: 1285,
                y: 6,
                width: 100,
                height: 50
            }
        );
        // A monitor left of the origin has a negative place, which the protocol's signed coordinates hold.
        let left_of = display("l", false, -1920, -100, 1920, 1080);
        assert_eq!(root_point(&left_of, 0, 0).unwrap(), (-1920, -100));
    }

    #[test]
    fn a_point_or_region_off_its_display_is_refused() {
        let state = two();
        let right = &state.displays[1];
        assert_eq!(
            root_point(right, 1024, 0).unwrap_err().code,
            ErrorCode::BadRequest
        );
        assert_eq!(
            root_point(right, -1, 0).unwrap_err().code,
            ErrorCode::BadRequest
        );
        assert_eq!(
            root_rect(right, Rect::new(1000, 0, 100, 10))
                .unwrap_err()
                .code,
            ErrorCode::BadRequest
        );
        let huge = display("huge", true, 40_000, 0, 100, 100);
        assert_eq!(
            root_point(&huge, 0, 0).unwrap_err().code,
            ErrorCode::BadRequest
        );
    }

    #[test]
    fn ids_come_from_names_and_stay_unique() {
        let mut taken = Vec::new();
        let first = display_id("HDMI-1", 0, &taken);
        assert_eq!(first, "hdmi-1");
        taken.push(first);
        assert_eq!(display_id("HDMI-1", 1, &taken), "hdmi-1-2");
        assert_eq!(display_id("", 2, &taken), "display3");
        assert_eq!(display_id("--", 3, &taken), "display4");
    }

    #[test]
    fn exactly_one_display_is_primary() {
        let mut displays = vec![
            display("a", false, 0, 0, 1, 1),
            display("b", true, 1, 0, 1, 1),
            display("c", true, 2, 0, 1, 1),
        ];
        one_primary(&mut displays);
        assert_eq!(
            displays.iter().map(|d| d.primary).collect::<Vec<_>>(),
            [false, true, false]
        );
        let mut none = vec![
            display("a", false, 0, 0, 1, 1),
            display("b", false, 1, 0, 1, 1),
        ];
        one_primary(&mut none);
        assert!(none[0].primary && !none[1].primary);
    }

    #[test]
    fn pixels_follow_the_servers_masks_and_byte_order() {
        // 32 bits, little endian, the usual layout: blue, green, red, unused.
        let little = PixelFormat {
            bits_per_pixel: 32,
            scanline_pad: 32,
            little_endian: true,
            red: 0x00ff_0000,
            green: 0x0000_ff00,
            blue: 0x0000_00ff,
        };
        let data = [3, 2, 1, 0, 30, 20, 10, 0];
        assert_eq!(rgb_of(&data, 2, 1, &little).unwrap(), [1, 2, 3, 10, 20, 30]);
        // The same pixels, big endian.
        let big = PixelFormat {
            little_endian: false,
            ..little
        };
        let data = [0, 1, 2, 3, 0, 10, 20, 30];
        assert_eq!(rgb_of(&data, 2, 1, &big).unwrap(), [1, 2, 3, 10, 20, 30]);
        // 16 bits (5-6-5) scale to 8, white stays white.
        let small = PixelFormat {
            bits_per_pixel: 16,
            scanline_pad: 16,
            little_endian: true,
            red: 0xf800,
            green: 0x07e0,
            blue: 0x001f,
        };
        assert_eq!(
            rgb_of(&[0xff, 0xff], 1, 1, &small).unwrap(),
            [255, 255, 255]
        );
        assert_eq!(rgb_of(&[0, 0], 1, 1, &small).unwrap(), [0, 0, 0]);
    }

    #[test]
    fn rows_are_read_at_the_servers_stride_and_a_short_picture_is_refused() {
        // 24 bits a pixel padded to 32: a row of 3 pixels is 12 bytes (9 used).
        let format = PixelFormat {
            bits_per_pixel: 24,
            scanline_pad: 32,
            little_endian: true,
            red: 0xff0000,
            green: 0xff00,
            blue: 0xff,
        };
        let row = |base: u8| {
            [
                base + 2,
                base + 1,
                base,
                base + 5,
                base + 4,
                base + 3,
                base + 8,
                base + 7,
                base + 6,
                0,
                0,
                0,
            ]
        };
        let mut data = Vec::new();
        data.extend_from_slice(&row(1));
        data.extend_from_slice(&row(100));
        let rgb = rgb_of(&data, 3, 2, &format).unwrap();
        assert_eq!(&rgb[..3], [1, 2, 3]);
        assert_eq!(&rgb[9..12], [100, 101, 102]);
        assert_eq!(
            rgb_of(&data[..20], 3, 2, &format).unwrap_err().code,
            ErrorCode::Io
        );
        let odd = PixelFormat {
            bits_per_pixel: 8,
            ..format
        };
        assert_eq!(
            rgb_of(&data, 3, 2, &odd).unwrap_err().code,
            ErrorCode::Unsupported
        );
    }

    #[test]
    fn a_window_contributes_the_part_of_it_inside_the_rectangle() {
        let at = RootRect {
            x: 100,
            y: 50,
            width: 40,
            height: 20,
        };
        // Inside, partly outside on each side, and not meeting.
        assert_eq!(overlap(at, 110, 55, 10, 5), Some((110, 55, 10, 5)));
        assert_eq!(overlap(at, 90, 40, 30, 20), Some((100, 50, 20, 10)));
        assert_eq!(overlap(at, 130, 60, 100, 100), Some((130, 60, 10, 10)));
        assert_eq!(overlap(at, 0, 0, 100, 50), None);
        assert_eq!(overlap(at, 140, 50, 5, 5), None);
        // A window placed far off the screen (as a compositor does before it places one) contributes nothing.
        assert_eq!(overlap(at, -32730, -32709, 128, 96), None);
    }

    #[test]
    fn a_piece_is_pasted_at_its_place_and_nowhere_else() {
        let mut canvas = vec![0_u8; 4 * 3 * 3];
        paste(&mut canvas, 4, (1, 1), &[9, 9, 9, 8, 8, 8], 2);
        let at = |x: usize, y: usize| canvas[(y * 4 + x) * 3];
        assert_eq!((at(1, 1), at(2, 1)), (9, 8));
        assert_eq!((at(0, 1), at(3, 1), at(1, 0), at(1, 2)), (0, 0, 0, 0));
        // A piece that would run past the canvas is clipped by its row, not written outside it.
        paste(&mut canvas, 4, (3, 2), &[7, 7, 7, 7, 7, 7], 2);
        assert_eq!(canvas.len(), 36);
    }

    #[test]
    fn a_key_the_keyboard_has_is_pressed_with_shift_when_it_is_the_second_symbol() {
        let map = map();
        assert_eq!(
            Stroke::of(&map, u32::from(b'a')).unwrap(),
            Stroke::Direct {
                code: 11,
                shift: None
            }
        );
        assert_eq!(
            Stroke::of(&map, u32::from(b'A')).unwrap(),
            Stroke::Direct {
                code: 11,
                shift: Some(14)
            }
        );
        let steps = key_steps(Stroke::of(&map, u32::from(b'A')).unwrap(), KeyAction::Click);
        assert_eq!(
            steps,
            [
                Step::Key {
                    code: 14,
                    down: true
                },
                Step::Key {
                    code: 11,
                    down: true
                },
                Step::Key {
                    code: 11,
                    down: false
                },
                Step::Key {
                    code: 14,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn a_character_with_no_key_is_typed_through_the_highest_spare_keycode_and_restored() {
        let map = map();
        let steps = text_steps("é", &map).unwrap();
        assert_eq!(
            steps,
            [
                Step::Remap {
                    code: 27,
                    sym: 0xe9,
                    per: 2
                },
                Step::Pause,
                Step::Key {
                    code: 27,
                    down: true
                },
                Step::Key {
                    code: 27,
                    down: false
                },
                Step::Pause,
                Step::Unmap { code: 27, per: 2 },
            ]
        );
    }

    #[test]
    fn a_keyboard_with_no_spare_key_cannot_type_what_it_has_no_key_for() {
        let full = KeyMap::new(8, 2, vec![1; 4]);
        assert_eq!(
            Stroke::of(&full, 0xe9).unwrap_err().code,
            ErrorCode::Unavailable
        );
        // A shifted symbol with no shift key on the keyboard falls back to a spare as well.
        let no_shift = KeyMap::new(8, 2, vec![u32::from(b'a'), u32::from(b'A'), 0, 0]);
        assert!(matches!(
            Stroke::of(&no_shift, u32::from(b'A')).unwrap(),
            Stroke::Spare { .. }
        ));
    }

    #[test]
    fn text_types_line_breaks_as_return_and_tabs_as_tab_and_names_resolve() {
        let map = map();
        let steps = text_steps("a\r\nb", &map).unwrap();
        let downs: Vec<u8> = steps
            .iter()
            .filter_map(|s| match s {
                Step::Key { code, down: true } => Some(*code),
                _ => None,
            })
            .collect();
        assert_eq!(downs, [11, 13, 12]);
        assert_eq!(keysym_of("enter"), Some(key::Return));
        assert_eq!(keysym_of("f24"), Some(key::F1 + 23));
        assert_eq!(keysym_of("f25"), None);
        assert_eq!(keysym_of("nope"), None);
        // A capital letter is the key of the letter: shift is its own event.
        assert_eq!(keysym_of("A"), Some(u32::from(b'a')));
        assert_eq!(keysym_of("é"), Some(0xe9));
    }

    #[test]
    fn a_click_is_a_press_and_a_release_and_the_wheel_uses_the_cores_buttons() {
        let state = two();
        let map = map();
        let click = InputEvent::Button {
            button: Button::Left,
            action: ButtonAction::Click,
            count: Some(2),
            display: Some(DisplayId("right".to_owned())),
            x: Some(10),
            y: Some(20),
        };
        let steps = plan(&click, &state, &map, 10).unwrap();
        assert_eq!(steps[0], Step::Motion { x: 1290, y: 20 });
        assert_eq!(steps.len(), 5);
        assert_eq!(
            steps[1],
            Step::Button {
                detail: 1,
                down: true
            }
        );
        let scroll = InputEvent::Scroll {
            display: None,
            x: None,
            y: None,
            dx: -1,
            dy: 2,
        };
        let steps = plan(&scroll, &state, &map, 10).unwrap();
        let details: Vec<u8> = steps
            .iter()
            .filter_map(|s| match s {
                Step::Button { detail, down: true } => Some(*detail),
                _ => None,
            })
            .collect();
        assert_eq!(details, [5, 5, 6]);
    }

    #[test]
    fn a_repeat_past_the_limit_and_a_point_off_the_display_are_refused() {
        let state = two();
        let map = map();
        let many = InputEvent::Button {
            button: Button::Left,
            action: ButtonAction::Click,
            count: Some(11),
            display: None,
            x: None,
            y: None,
        };
        assert_eq!(
            plan(&many, &state, &map, 10).unwrap_err().code,
            ErrorCode::BadRequest
        );
        let off = InputEvent::Move {
            display: None,
            x: 1280,
            y: 0,
        };
        assert_eq!(
            plan(&off, &state, &map, 10).unwrap_err().code,
            ErrorCode::BadRequest
        );
    }
}
