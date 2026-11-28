//! What the macOS backend's answers mean and how `bohio-gui/1` input becomes the operations the operating system takes:
//! the display list, a pixel's point, the key table, text as key strokes, the login session's state and the pictures'
//! byte layout. Pure, so every rule is tested on every platform; `macos.rs` asks the operating system and hands the
//! answers here.

#![forbid(unsafe_code)]

use std::path::Path;

use bohio_proto::gui::{Button, ButtonAction, Display, DisplayId, InputEvent, KeyAction};

use bohio_x11::{DesktopState, GuiError};

/// What a node says when no one is logged in at the screen: ScreenCaptureKit and the event system need a window server
/// session, which a login at the screen (or a LaunchAgent in that session) has and a LaunchDaemon or an ssh login lacks.
pub(crate) const NO_SESSION: &str = "no one is logged in at this Mac's screen: a window server session is needed to \
     capture or drive it, so run the node as the logged-in user (a LaunchAgent, or a terminal in that session)";

/// What `Screen Recording` being missing says: the pane to grant it in and the program the grant is for.
pub(crate) fn screen_recording_denied(binary: &Path) -> String {
    format!(
        "this program may not record the screen: allow it in System Settings, Privacy & Security, Screen Recording \
         (Screen & System Audio Recording on macOS 15 and later), for {}; a grant belongs to the program at that \
         path, so a moved or replaced program must be allowed again",
        binary.display()
    )
}

/// What `Accessibility` being missing says: the pane to grant it in and the program the grant is for.
pub(crate) fn accessibility_denied(binary: &Path) -> String {
    format!(
        "this program may not send keyboard and mouse input: allow it in System Settings, Privacy & Security, \
         Accessibility, for {}; a grant belongs to the program at that path, so a moved or replaced program must be \
         allowed again",
        binary.display()
    )
}

/// What the operating system said of one display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RawDisplay {
    /// The `CGDirectDisplayID`.
    pub id: u32,
    /// `CGDisplayIsMain`.
    pub main: bool,
    /// Its place in the global coordinate space, in points: the main display's top-left is the origin, `y` grows down.
    pub x: f64,
    pub y: f64,
    /// Its size in points.
    pub width: f64,
    pub height: f64,
    /// Its size in backing pixels, as the current display mode has it.
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// `CGDisplayRotation`, in degrees.
    pub rotation: f64,
}

/// The displays `bohio-gui/1` shows. A display's id is `display` and its `CGDirectDisplayID`; `x` and `y` are its place
/// in the layout in points (the system's own layout space); `width` and `height` are backing pixels and `scale` is pixels
/// a point, so a pixel of a snapshot is `1 / scale` of a point. The display `CGDisplayIsMain` names is the primary one.
pub(crate) fn displays_of(raw: &[RawDisplay]) -> Vec<Display> {
    let mut displays: Vec<Display> = raw
        .iter()
        .filter(|display| display.pixel_width > 0 && display.pixel_height > 0)
        .map(|display| Display {
            id: DisplayId(format!("display{}", display.id)),
            name: None,
            primary: display.main,
            x: whole(display.x),
            y: whole(display.y),
            width: display.pixel_width,
            height: display.pixel_height,
            scale: if display.width > 0.0 {
                f64::from(display.pixel_width) / display.width
            } else {
                1.0
            },
            rotation: rotation_of(display.rotation),
        })
        .collect();
    // Exactly one display is the default.
    if let Some(first) = displays.iter().position(|display| display.primary) {
        for (index, display) in displays.iter_mut().enumerate() {
            display.primary = index == first;
        }
    } else if let Some(display) = displays.first_mut() {
        display.primary = true;
    }
    displays
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "a display's place is a few thousand points"
)]
fn whole(points: f64) -> i32 {
    points
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a rotation is held to 0 to 359 first"
)]
fn rotation_of(degrees: f64) -> Option<u32> {
    degrees
        .is_finite()
        .then(|| degrees.rem_euclid(360.0).round() as u32 % 360)
}

/// What the operating system said of the login session (`CGSessionCopyCurrentDictionary`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionFacts {
    /// `kCGSSessionOnConsoleKey`: this session is the one at the screen. `None` when the dictionary lacks the key.
    pub on_console: Option<bool>,
    /// `CGSSessionScreenIsLocked`: present (and true) only while the screen is locked.
    pub screen_locked: Option<bool>,
}

/// The state of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Judged {
    pub active: bool,
    pub locked: bool,
}

/// The state of the session, or why there is none.
///
/// # Errors
///
/// `unavailable` when the system has no login session for this process.
pub(crate) fn judge(facts: Option<SessionFacts>) -> Result<Judged, GuiError> {
    let facts = facts.ok_or_else(|| GuiError::unavailable(NO_SESSION))?;
    Ok(Judged {
        // A dictionary without the key is not a reason to refuse: a session that is not at the screen says so.
        active: facts.on_console != Some(false),
        // The key is absent while the screen is not locked.
        locked: facts.screen_locked == Some(true),
    })
}

/// The point of the global coordinate space a pixel of a display is on: the display's own place plus the pixel over the
/// display's scale, rounded down to the point that holds the pixel (the event system takes whole points).
///
/// # Errors
///
/// `bad_request` for a pixel off the display.
pub(crate) fn point_of(display: &Display, x: i32, y: i32) -> Result<(i32, i32), GuiError> {
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
    let scale = if display.scale > 0.0 {
        display.scale
    } else {
        1.0
    };
    let along =
        |origin: i32, pixel: i32| origin.saturating_add(whole((f64::from(pixel) / scale).floor()));
    Ok((along(display.x, x), along(display.y, y)))
}

/// The pixel of a display a point of the global space is on, or `None` when it lies on none of them.
pub(crate) fn pixel_of(displays: &[Display], x: i32, y: i32) -> Option<(DisplayId, i32, i32)> {
    displays.iter().find_map(|display| {
        let scale = if display.scale > 0.0 {
            display.scale
        } else {
            1.0
        };
        let (px, py) = (
            (f64::from(x) - f64::from(display.x)) * scale,
            (f64::from(y) - f64::from(display.y)) * scale,
        );
        let inside = px >= 0.0
            && py >= 0.0
            && px < f64::from(display.width)
            && py < f64::from(display.height);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the point is inside a display, a few thousand pixels"
        )]
        inside.then(|| (display.id.clone(), px as i32, py as i32))
    })
}

/// A key that has no character: the names of `bohio-gui/1`'s closed list that the Mac's keyboard has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Named {
    Enter,
    Tab,
    Escape,
    Backspace,
    /// The forward delete key (`fn` + delete on a laptop).
    Delete,
    /// Where a PC keyboard's Insert is: the Help key.
    Help,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    Space,
    /// F1 to F20.
    Function(u8),
    Shift,
    Control,
    /// Option.
    Alt,
    /// Command.
    Meta,
    CapsLock,
}

/// What a key event presses: a named key, or a character as the keyboard layout has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Named(Named),
    Char(char),
}

/// The key `name` is.
///
/// # Errors
///
/// `bad_request` for a name that is not a key; `unsupported` for a key of the closed list that a Mac's keyboard lacks
/// (`print_screen`, `menu`, `f21` to `f24`).
pub(crate) fn key_of(name: &str) -> Result<Key, GuiError> {
    let mut characters = name.chars();
    if let (Some(only), None) = (characters.next(), characters.next()) {
        return Ok(Key::Char(only));
    }
    let named = match name {
        "enter" => Named::Enter,
        "tab" => Named::Tab,
        "escape" => Named::Escape,
        "backspace" => Named::Backspace,
        "delete" => Named::Delete,
        "insert" => Named::Help,
        "home" => Named::Home,
        "end" => Named::End,
        "page_up" => Named::PageUp,
        "page_down" => Named::PageDown,
        "up" => Named::Up,
        "down" => Named::Down,
        "left" => Named::Left,
        "right" => Named::Right,
        "space" => Named::Space,
        "shift" => Named::Shift,
        "control" => Named::Control,
        "alt" => Named::Alt,
        "meta" => Named::Meta,
        "caps_lock" => Named::CapsLock,
        "print_screen" | "menu" => {
            return Err(GuiError::unsupported("a Mac's keyboard has no such key"));
        }
        other => {
            let number: u8 = other
                .strip_prefix('f')
                .and_then(|digits| digits.parse().ok())
                .ok_or_else(|| GuiError::bad_request("that is not a key name"))?;
            match number {
                1..=20 => Named::Function(number),
                21..=24 => {
                    return Err(GuiError::unsupported(
                        "a Mac's keyboard has function keys up to F20",
                    ));
                }
                _ => return Err(GuiError::bad_request("that is not a key name")),
            }
        }
    };
    Ok(Key::Named(named))
}

/// One operation of the event system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Put the pointer on a point of the global space.
    Move { x: i32, y: i32 },
    /// Press or release a button.
    Button { button: Button, down: bool },
    /// Scroll by `notches` lines: down (positive) or up along the vertical axis, right or left along the other.
    Scroll { notches: i32, horizontal: bool },
    /// Press or release a key.
    Key { key: Key, action: KeyAction },
    /// Type characters as they are, whatever the layout. Never a line break or a tab: those are keys.
    Text(String),
}

/// The operations that type `text`: the characters as themselves, a line break as Return and a tab as Tab, which
/// applications take for those keys and not for a character.
pub(crate) fn text_steps(text: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut run = String::new();
    let mut characters = text.chars().peekable();
    let flush = |run: &mut String, steps: &mut Vec<Step>| {
        if !run.is_empty() {
            steps.push(Step::Text(std::mem::take(run)));
        }
    };
    while let Some(character) = characters.next() {
        let named = match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                Some(Named::Enter)
            }
            '\n' => Some(Named::Enter),
            '\t' => Some(Named::Tab),
            other => {
                run.push(other);
                None
            }
        };
        if let Some(named) = named {
            flush(&mut run, &mut steps);
            steps.push(Step::Key {
                key: Key::Named(named),
                action: KeyAction::Click,
            });
        }
    }
    flush(&mut run, &mut steps);
    steps
}

/// The operations of one event, in order. `wait` has none (the engine pauses itself).
///
/// # Errors
///
/// `bad_request` for a point off its display, a key that is not one or a repeat past `max_repeat`; `unsupported` for a
/// key a Mac's keyboard lacks.
pub(crate) fn plan(
    event: &InputEvent,
    state: &DesktopState,
    max_repeat: u32,
) -> Result<Vec<Step>, GuiError> {
    let motion = |display: Option<&DisplayId>, x: i32, y: i32| -> Result<Step, GuiError> {
        let (x, y) = point_of(state.display(display)?, x, y)?;
        Ok(Step::Move { x, y })
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
            match action {
                ButtonAction::Press => steps.push(Step::Button {
                    button: *button,
                    down: true,
                }),
                ButtonAction::Release => steps.push(Step::Button {
                    button: *button,
                    down: false,
                }),
                ButtonAction::Click => {
                    for _ in 0..repeat(count.unwrap_or(1).max(1))? {
                        for down in [true, false] {
                            steps.push(Step::Button {
                                button: *button,
                                down,
                            });
                        }
                    }
                }
                _ => {
                    return Err(GuiError::bad_request(
                        "this host does not take that button action",
                    ));
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
            // The wire's positive `dy` scrolls down and positive `dx` right, as the event system's own.
            for (notches, horizontal) in [(*dy, false), (*dx, true)] {
                if notches != 0 {
                    repeat(notches.unsigned_abs())?;
                    steps.push(Step::Scroll {
                        notches,
                        horizontal,
                    });
                }
            }
            steps
        }
        InputEvent::Key { key, action } => vec![Step::Key {
            key: key_of(key)?,
            action: *action,
        }],
        InputEvent::Text { text } => text_steps(text),
        InputEvent::Wait { .. } => Vec::new(),
        _ => return Err(GuiError::bad_request("this host does not take that input")),
    })
}

/// Where the colours are in one four-byte pixel of a picture the system hands over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Packed {
    pub red: usize,
    pub green: usize,
    pub blue: usize,
}

/// The byte positions of red, green and blue in a pixel of 8-bit colour, 32 bits a pixel, as a `CGImage` describes its
/// layout (`CGBitmapInfo`: the alpha position in the low bits, the byte order of the 32-bit word in bits 12 to 14).
/// Whether the alpha is premultiplied does not matter: the screen is opaque.
///
/// # Errors
///
/// `unsupported` for any other layout (16-bit or floating-point colour, indexed or gray pictures).
pub(crate) fn packed_of(
    bits_per_component: usize,
    bits_per_pixel: usize,
    bitmap_info: u32,
) -> Result<Packed, GuiError> {
    const ALPHA_MASK: u32 = 0x1F;
    const ORDER_MASK: u32 = 0x7000;
    const LITTLE_32: u32 = 0x2000;
    const DEFAULT_ORDER: u32 = 0;
    const BIG_32: u32 = 0x4000;
    if bits_per_component != 8 || bits_per_pixel != 32 {
        return Err(GuiError::unsupported(
            "the screen's pixels are not 8-bit colour with four bytes a pixel",
        ));
    }
    // CGImageAlphaInfo: 1 premultiplied last, 2 premultiplied first, 3 last, 4 first, 5 none skip last, 6 none skip first.
    let first = match bitmap_info & ALPHA_MASK {
        2 | 4 | 6 => true,
        1 | 3 | 5 => false,
        _ => {
            return Err(GuiError::unsupported(
                "the screen's pixels have an alpha layout this host does not read",
            ));
        }
    };
    let packed = |red, green, blue| Packed { red, green, blue };
    match (bitmap_info & ORDER_MASK, first) {
        // The 32-bit word is read little-endian: ARGB in the word is B, G, R, A in memory.
        (LITTLE_32, true) => Ok(packed(2, 1, 0)),
        (LITTLE_32, false) => Ok(packed(3, 2, 1)),
        (DEFAULT_ORDER | BIG_32, true) => Ok(packed(1, 2, 3)),
        (DEFAULT_ORDER | BIG_32, false) => Ok(packed(0, 1, 2)),
        _ => Err(GuiError::unsupported(
            "the screen's pixels have a byte order this host does not read",
        )),
    }
}

/// How many bytes of a `CFData`'s byte pointer may be read as a slice: `length`, unless the data is empty or the
/// pointer is null (`CFDataGetBytePtr` of empty data may be), which no slice can be made of.
///
/// # Errors
///
/// An I/O error when there is nothing to read.
pub(crate) fn readable_pixels(pointer: *const u8, length: usize) -> Result<usize, GuiError> {
    if pointer.is_null() || length == 0 {
        return Err(GuiError::io("the screenshot's pixels could not be read"));
    }
    Ok(length)
}

/// The RGB bytes of a picture of four-byte pixels laid out as `packed` says, rows `stride` bytes apart, cut to `region`
/// (left, top, width, height in pixels).
///
/// # Errors
///
/// `internal` when the rows or the region do not fit the picture.
pub(crate) fn rgb_of_packed(
    rows: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    packed: Packed,
    region: (usize, usize, usize, usize),
) -> Result<Vec<u8>, GuiError> {
    let (left, top, region_width, region_height) = region;
    let fits = region_width > 0
        && region_height > 0
        && left
            .checked_add(region_width)
            .is_some_and(|end| end <= width)
        && top
            .checked_add(region_height)
            .is_some_and(|end| end <= height)
        && stride >= width.saturating_mul(4)
        && rows.len() >= stride.saturating_mul(height);
    if !fits {
        return Err(GuiError::new(
            bohio_proto::ErrorCode::Internal,
            "the system returned a picture that does not match its size",
        ));
    }
    let mut rgb = Vec::with_capacity(region_width * region_height * 3);
    for row in top..top + region_height {
        let line = &rows[row * stride + left * 4..row * stride + (left + region_width) * 4];
        for pixel in line.as_chunks::<4>().0 {
            rgb.extend_from_slice(&[pixel[packed.red], pixel[packed.green], pixel[packed.blue]]);
        }
    }
    Ok(rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(
        id: u32,
        main: bool,
        x: f64,
        y: f64,
        points: (f64, f64),
        pixels: (u32, u32),
    ) -> RawDisplay {
        RawDisplay {
            id,
            main,
            x,
            y,
            width: points.0,
            height: points.1,
            pixel_width: pixels.0,
            pixel_height: pixels.1,
            rotation: 0.0,
        }
    }

    /// A retina main display and a plain one to its right, a second one above it at an offset.
    fn two() -> DesktopState {
        let mut state = DesktopState::default();
        state.displays = displays_of(&[
            raw(1, true, 0.0, 0.0, (1512.0, 982.0), (3024, 1964)),
            raw(7, false, 1512.0, -300.0, (1920.0, 1080.0), (1920, 1080)),
        ]);
        state
    }

    #[test]
    fn displays_carry_pixels_a_point_scale_and_one_primary() {
        let state = two();
        let main = &state.displays[0];
        assert_eq!(main.id, DisplayId("display1".to_owned()));
        assert_eq!((main.width, main.height), (3024, 1964));
        assert!((main.scale - 2.0).abs() < f64::EPSILON);
        assert_eq!((main.x, main.y), (0, 0));
        let second = &state.displays[1];
        assert!((second.scale - 1.0).abs() < f64::EPSILON);
        assert_eq!((second.x, second.y), (1512, -300));
        assert_eq!(second.rotation, Some(0));
        assert_eq!(
            state
                .displays
                .iter()
                .filter(|display| display.primary)
                .count(),
            1
        );
        // No display says it is the main one: the first is the default. Two that do: the first of them.
        let none = displays_of(&[
            raw(3, false, 0.0, 0.0, (10.0, 10.0), (10, 10)),
            raw(4, false, 10.0, 0.0, (10.0, 10.0), (10, 10)),
        ]);
        assert_eq!(none.iter().filter(|display| display.primary).count(), 1);
        assert!(none[0].primary);
        let both = displays_of(&[
            raw(3, true, 0.0, 0.0, (10.0, 10.0), (10, 10)),
            raw(4, true, 10.0, 0.0, (10.0, 10.0), (10, 10)),
        ]);
        assert!(both[0].primary && !both[1].primary);
    }

    #[test]
    fn a_display_with_no_pixels_is_not_listed() {
        let listed = displays_of(&[raw(9, true, 0.0, 0.0, (10.0, 10.0), (0, 0))]);
        assert!(listed.is_empty());
    }

    #[test]
    fn rotation_is_held_to_a_turn() {
        let mut turned = raw(2, true, 0.0, 0.0, (10.0, 10.0), (10, 10));
        turned.rotation = 270.0;
        assert_eq!(displays_of(&[turned])[0].rotation, Some(270));
        turned.rotation = -90.0;
        assert_eq!(displays_of(&[turned])[0].rotation, Some(270));
        turned.rotation = f64::NAN;
        assert_eq!(displays_of(&[turned])[0].rotation, None);
    }

    #[test]
    fn a_pixel_is_the_point_that_holds_it_on_its_own_display() {
        let state = two();
        let retina = &state.displays[0];
        // Two pixels a point: pixel 101 is in point 50.
        assert_eq!(point_of(retina, 100, 51).unwrap(), (50, 25));
        assert_eq!(point_of(retina, 101, 50).unwrap(), (50, 25));
        assert_eq!(point_of(retina, 3023, 1963).unwrap(), (1511, 981));
        // The second display's pixels add its own place, which lies right of and above the main one's.
        let second = &state.displays[1];
        assert_eq!(point_of(second, 0, 0).unwrap(), (1512, -300));
        assert_eq!(point_of(second, 10, 20).unwrap(), (1522, -280));
    }

    #[test]
    fn a_point_off_its_display_is_refused() {
        let state = two();
        for (x, y) in [(-1, 0), (0, -1), (3024, 0), (0, 1964)] {
            let error = point_of(&state.displays[0], x, y).unwrap_err();
            assert_eq!(error.code, bohio_proto::ErrorCode::BadRequest, "{x} {y}");
            assert!(error.message.contains("3024 by 1964"), "{error}");
        }
    }

    #[test]
    fn a_point_of_the_global_space_is_the_pixel_of_its_display() {
        let state = two();
        let (id, x, y) = pixel_of(&state.displays, 50, 25).unwrap();
        assert_eq!((id, x, y), (DisplayId("display1".to_owned()), 100, 50));
        let (id, x, y) = pixel_of(&state.displays, 1522, -280).unwrap();
        assert_eq!((id, x, y), (DisplayId("display7".to_owned()), 10, 20));
        // Between the displays' edges there is nothing.
        assert!(pixel_of(&state.displays, 1600, 2000).is_none());
        assert!(pixel_of(&state.displays, -1, 0).is_none());
    }

    #[test]
    fn a_session_not_at_the_screen_is_not_active_and_a_locked_screen_says_so() {
        let facts = |on_console, screen_locked| {
            judge(Some(SessionFacts {
                on_console,
                screen_locked,
            }))
            .unwrap()
        };
        assert_eq!(
            facts(Some(true), None),
            Judged {
                active: true,
                locked: false
            }
        );
        assert!(facts(Some(true), Some(true)).locked);
        assert!(!facts(Some(true), Some(false)).locked);
        assert!(!facts(Some(false), None).active);
        assert!(facts(None, None).active);
    }

    #[test]
    fn no_session_is_unavailable_and_says_what_to_do() {
        let error = judge(None).unwrap_err();
        assert_eq!(error.code, bohio_proto::ErrorCode::Unavailable);
        assert!(error.message.contains("LaunchAgent"), "{error}");
    }

    #[test]
    fn a_missing_permission_names_the_pane_and_the_binary() {
        let binary = Path::new("/Applications/Bohio/bin/bohio");
        let screen = screen_recording_denied(binary);
        for word in [
            "System Settings",
            "Privacy & Security",
            "Screen Recording",
            "/Applications/Bohio/bin/bohio",
        ] {
            assert!(screen.contains(word), "{word}: {screen}");
        }
        let input = accessibility_denied(binary);
        for word in [
            "System Settings",
            "Privacy & Security",
            "Accessibility",
            "/Applications/Bohio/bin/bohio",
        ] {
            assert!(input.contains(word), "{word}: {input}");
        }
    }

    #[test]
    fn the_closed_list_of_keys_resolves_and_what_a_mac_lacks_is_refused_as_such() {
        assert_eq!(key_of("enter").unwrap(), Key::Named(Named::Enter));
        assert_eq!(key_of("meta").unwrap(), Key::Named(Named::Meta));
        assert_eq!(key_of("alt").unwrap(), Key::Named(Named::Alt));
        assert_eq!(key_of("insert").unwrap(), Key::Named(Named::Help));
        assert_eq!(key_of("f1").unwrap(), Key::Named(Named::Function(1)));
        assert_eq!(key_of("f20").unwrap(), Key::Named(Named::Function(20)));
        assert_eq!(key_of("é").unwrap(), Key::Char('é'));
        for name in ["print_screen", "menu", "f21", "f24"] {
            assert_eq!(
                key_of(name).unwrap_err().code,
                bohio_proto::ErrorCode::Unsupported,
                "{name}"
            );
        }
        for name in ["f0", "f25", "ctrl", "fx", ""] {
            assert_eq!(
                key_of(name).unwrap_err().code,
                bohio_proto::ErrorCode::BadRequest,
                "{name:?}"
            );
        }
    }

    #[test]
    fn text_types_characters_whole_and_line_breaks_and_tabs_as_keys() {
        let enter = Step::Key {
            key: Key::Named(Named::Enter),
            action: KeyAction::Click,
        };
        let tab = Step::Key {
            key: Key::Named(Named::Tab),
            action: KeyAction::Click,
        };
        assert_eq!(
            text_steps("hello é"),
            vec![Step::Text("hello é".to_owned())]
        );
        assert_eq!(
            text_steps("a\nb\r\nc\td\re"),
            vec![
                Step::Text("a".to_owned()),
                enter.clone(),
                Step::Text("b".to_owned()),
                enter.clone(),
                Step::Text("c".to_owned()),
                tab,
                Step::Text("d".to_owned()),
                enter,
                Step::Text("e".to_owned()),
            ]
        );
        assert!(text_steps("").is_empty());
    }

    #[test]
    fn a_click_is_a_press_and_a_release_and_a_point_comes_first() {
        let state = two();
        let steps = plan(
            &InputEvent::Button {
                button: Button::Left,
                action: ButtonAction::Click,
                count: Some(2),
                display: Some(DisplayId("display7".to_owned())),
                x: Some(10),
                y: Some(20),
            },
            &state,
            1000,
        )
        .unwrap();
        let button = |down| Step::Button {
            button: Button::Left,
            down,
        };
        assert_eq!(
            steps,
            vec![
                Step::Move { x: 1522, y: -280 },
                button(true),
                button(false),
                button(true),
                button(false),
            ]
        );
    }

    #[test]
    fn the_wheel_scrolls_each_axis_by_its_notches_with_the_wires_signs() {
        let state = two();
        let steps = plan(
            &InputEvent::Scroll {
                display: None,
                x: None,
                y: None,
                dx: -2,
                dy: 3,
            },
            &state,
            1000,
        )
        .unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Scroll {
                    notches: 3,
                    horizontal: false
                },
                Step::Scroll {
                    notches: -2,
                    horizontal: true
                },
            ]
        );
    }

    #[test]
    fn a_repeat_past_the_limit_and_a_point_off_the_display_refuse_the_event() {
        let state = two();
        let click = |count, x| InputEvent::Button {
            button: Button::Left,
            action: ButtonAction::Click,
            count: Some(count),
            display: None,
            x: Some(x),
            y: Some(0),
        };
        assert_eq!(
            plan(&click(5, 0), &state, 4).unwrap_err().code,
            bohio_proto::ErrorCode::BadRequest
        );
        assert!(plan(&click(4, 0), &state, 4).is_ok());
        assert_eq!(
            plan(&click(1, 99_999), &state, 4).unwrap_err().code,
            bohio_proto::ErrorCode::BadRequest
        );
        let scroll = InputEvent::Scroll {
            display: None,
            x: None,
            y: None,
            dx: 0,
            dy: 1_000_000,
        };
        assert_eq!(
            plan(&scroll, &state, 1000).unwrap_err().code,
            bohio_proto::ErrorCode::BadRequest
        );
    }

    #[test]
    fn pixels_are_cut_to_the_region_and_lose_the_alpha_byte() {
        let layout = Packed {
            red: 0,
            green: 1,
            blue: 2,
        };
        // Three by two pixels, rows padded to 16 bytes.
        let mut rows = vec![0xEE_u8; 32];
        for (index, pixel) in (0..6_u8).enumerate() {
            let at = (index / 3) * 16 + (index % 3) * 4;
            rows[at..at + 4].copy_from_slice(&[pixel, pixel + 10, pixel + 20, 0xFF]);
        }
        let all = rgb_of_packed(&rows, 16, 3, 2, layout, (0, 0, 3, 2)).unwrap();
        assert_eq!(all.len(), 18);
        assert_eq!(&all[..6], &[0, 10, 20, 1, 11, 21]);
        let corner = rgb_of_packed(&rows, 16, 3, 2, layout, (1, 1, 2, 1)).unwrap();
        assert_eq!(corner, vec![4, 14, 24, 5, 15, 25]);
    }

    #[test]
    fn pixels_are_readable_only_through_a_pointer_to_something() {
        let bytes = [1_u8, 2, 3, 4];
        assert_eq!(readable_pixels(bytes.as_ptr(), bytes.len()).unwrap(), 4);
        for (pointer, length) in [
            (std::ptr::null(), 0),
            (std::ptr::null(), 4),
            (bytes.as_ptr(), 0),
        ] {
            let error = readable_pixels(pointer, length).unwrap_err();
            assert_eq!(error.code, GuiError::io("x").code);
        }
    }

    #[test]
    fn a_picture_that_does_not_fit_its_size_is_refused() {
        let layout = Packed {
            red: 0,
            green: 1,
            blue: 2,
        };
        let rows = vec![0_u8; 32];
        for (stride, width, height, region) in [
            (16, 3, 2, (0, 0, 4, 2)),
            (16, 3, 2, (2, 0, 2, 1)),
            (16, 3, 2, (0, 0, 0, 1)),
            (8, 3, 2, (0, 0, 3, 2)),
            (16, 3, 3, (0, 0, 3, 3)),
        ] {
            let error = rgb_of_packed(&rows, stride, width, height, layout, region).unwrap_err();
            assert_eq!(error.code, bohio_proto::ErrorCode::Internal);
        }
    }

    #[test]
    fn the_layout_of_a_screenshot_says_where_each_colour_is() {
        // What ScreenCaptureKit hands over: premultiplied first, 32-bit little-endian words (B, G, R, A in memory).
        let screen = packed_of(8, 32, 2 | 0x2000).unwrap();
        assert_eq!(
            screen,
            Packed {
                red: 2,
                green: 1,
                blue: 0
            }
        );
        let rows = [10_u8, 20, 30, 255];
        assert_eq!(
            rgb_of_packed(&rows, 4, 1, 1, screen, (0, 0, 1, 1)).unwrap(),
            vec![30, 20, 10]
        );
        // Skip-first and default order: A, R, G, B. Last and little: A, B, G, R. Last and big: R, G, B, A.
        assert_eq!(
            packed_of(8, 32, 6).unwrap(),
            Packed {
                red: 1,
                green: 2,
                blue: 3
            }
        );
        assert_eq!(
            packed_of(8, 32, 1 | 0x2000).unwrap(),
            Packed {
                red: 3,
                green: 2,
                blue: 1
            }
        );
        assert_eq!(
            packed_of(8, 32, 5 | 0x4000).unwrap(),
            Packed {
                red: 0,
                green: 1,
                blue: 2
            }
        );
    }

    #[test]
    fn a_layout_this_host_does_not_read_is_unsupported() {
        for (component, pixel, info) in [
            (16, 64, 1),
            (8, 24, 0),
            (8, 32, 0),
            (8, 32, 2 | 0x1000),
            (8, 32, 7),
        ] {
            let error = packed_of(component, pixel, info).unwrap_err();
            assert_eq!(error.code, bohio_proto::ErrorCode::Unsupported, "{info}");
        }
    }
}
