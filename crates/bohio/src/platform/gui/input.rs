//! `bohio-gui/1` input events as the operations `SendInput` takes: coordinates normalised to the virtual screen, the
//! virtual-key table, text as key strokes. Pure, so every rule is tested on every platform; the Windows backend turns an
//! [`Op`] into an `INPUT` and sends it.
//!
//! The flag and key values are Windows' documented ABI constants, written out here and checked against the `windows`
//! crate's by `windows.rs` at compile time.

#![forbid(unsafe_code)]

use bohio_proto::gui::{Button, ButtonAction, Display, InputEvent, KeyAction};

use crate::gui::{DesktopState, GuiError};

pub(crate) const MOUSEEVENTF_MOVE: u32 = 0x0001;
pub(crate) const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
pub(crate) const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
pub(crate) const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
pub(crate) const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
pub(crate) const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
pub(crate) const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
pub(crate) const MOUSEEVENTF_XDOWN: u32 = 0x0080;
pub(crate) const MOUSEEVENTF_XUP: u32 = 0x0100;
pub(crate) const MOUSEEVENTF_WHEEL: u32 = 0x0800;
pub(crate) const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
pub(crate) const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
pub(crate) const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
pub(crate) const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
pub(crate) const KEYEVENTF_KEYUP: u32 = 0x0002;
pub(crate) const KEYEVENTF_UNICODE: u32 = 0x0004;
pub(crate) const XBUTTON1: u32 = 1;
pub(crate) const XBUTTON2: u32 = 2;
/// One wheel notch.
pub(crate) const WHEEL_DELTA: i32 = 120;

const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0D;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_CAPITAL: u16 = 0x14;
const VK_ESCAPE: u16 = 0x1B;
const VK_SPACE: u16 = 0x20;
const VK_PRIOR: u16 = 0x21;
const VK_NEXT: u16 = 0x22;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_SNAPSHOT: u16 = 0x2C;
const VK_INSERT: u16 = 0x2D;
const VK_DELETE: u16 = 0x2E;
const VK_LWIN: u16 = 0x5B;
const VK_APPS: u16 = 0x5D;
const VK_F1: u16 = 0x70;

/// What one `SendInput` structure holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    /// A mouse event: `x` and `y` (normalised, 0 to 65535, when the flags say absolute), `data` (the wheel amount or the
    /// extra button) and the `MOUSEEVENTF_*` flags.
    Mouse {
        x: i32,
        y: i32,
        data: u32,
        flags: u32,
    },
    /// A keyboard event: a virtual key (`vk` not 0), or one UTF-16 unit (`vk` 0) with `KEYEVENTF_UNICODE` in the flags.
    Key { vk: u16, unit: u16, flags: u32 },
}

/// The virtual screen: the rectangle that spans every display, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Virtual {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Virtual {
    /// The rectangle that holds every display, or `None` for no display.
    pub(crate) fn of(displays: &[Display]) -> Option<Self> {
        let mut found: Option<(i64, i64, i64, i64)> = None;
        for display in displays {
            let (left, top) = (i64::from(display.x), i64::from(display.y));
            let (right, bottom) = (
                left + i64::from(display.width),
                top + i64::from(display.height),
            );
            found = Some(match found {
                None => (left, top, right, bottom),
                Some((l, t, r, b)) => (l.min(left), t.min(top), r.max(right), b.max(bottom)),
            });
        }
        let (left, top, right, bottom) = found?;
        Some(Self {
            x: i32::try_from(left).ok()?,
            y: i32::try_from(top).ok()?,
            width: u32::try_from(right - left).ok()?,
            height: u32::try_from(bottom - top).ok()?,
        })
    }

    /// The absolute coordinates `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` takes for the pixel `x`, `y` of the
    /// virtual screen: the 0 to 65535 range spans the whole of it, every display, not the main one. A point outside is
    /// held to the nearest edge.
    pub(crate) fn normalise(self, x: i32, y: i32) -> (i32, i32) {
        (
            scale(i64::from(x) - i64::from(self.x), self.width),
            scale(i64::from(y) - i64::from(self.y), self.height),
        )
    }

    /// The pixel Windows puts the pointer on for the absolute coordinates `x`, `y`.
    #[cfg(test)]
    pub(crate) fn denormalise(self, x: i32, y: i32) -> (i32, i32) {
        let axis = |n: i32, origin: i32, extent: u32| {
            origin + i32::try_from(i64::from(n) * i64::from(extent) / 65536).unwrap_or(i32::MAX)
        };
        (axis(x, self.x, self.width), axis(y, self.y, self.height))
    }
}

/// `ceil(offset * 65536 / extent)`, with `offset` held inside the extent: the smallest absolute coordinate that
/// Windows maps back (rounding down) to that pixel.
fn scale(offset: i64, extent: u32) -> i32 {
    let extent = i64::from(extent.max(1));
    let offset = offset.clamp(0, extent - 1);
    i32::try_from(((offset * 65536 + extent - 1) / extent).min(65535)).unwrap_or(65535)
}

/// What a request's key resolves to: a virtual key, or one UTF-16 unit typed as itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stroke {
    Vk { code: u16, extended: bool },
    Unit(u16),
}

const fn plain(code: u16) -> Stroke {
    Stroke::Vk {
        code,
        extended: false,
    }
}

const fn extended(code: u16) -> Stroke {
    Stroke::Vk {
        code,
        extended: true,
    }
}

/// The keys `key` names, or `None` for a name that is not a key. A name of the closed list is its virtual key; one
/// ASCII letter or digit is the key of that letter, in no case (shift is its own event); any other character is typed as
/// the character, whatever the layout.
pub(crate) fn strokes_of(key: &str) -> Option<Vec<Stroke>> {
    let mut characters = key.chars();
    let (Some(first), None) = (characters.next(), characters.next()) else {
        return named(key).map(|stroke| vec![stroke]);
    };
    if first.is_ascii_alphabetic() {
        return Some(vec![plain(u16::from(first.to_ascii_uppercase() as u8))]);
    }
    if first.is_ascii_digit() {
        return Some(vec![plain(u16::from(first as u8))]);
    }
    if first == ' ' {
        return Some(vec![plain(VK_SPACE)]);
    }
    let mut units = [0_u16; 2];
    Some(
        first
            .encode_utf16(&mut units)
            .iter()
            .map(|unit| Stroke::Unit(*unit))
            .collect(),
    )
}

fn named(name: &str) -> Option<Stroke> {
    Some(match name {
        "enter" => plain(VK_RETURN),
        "tab" => plain(VK_TAB),
        "escape" => plain(VK_ESCAPE),
        "backspace" => plain(VK_BACK),
        "delete" => extended(VK_DELETE),
        "insert" => extended(VK_INSERT),
        "home" => extended(VK_HOME),
        "end" => extended(VK_END),
        "page_up" => extended(VK_PRIOR),
        "page_down" => extended(VK_NEXT),
        "up" => extended(VK_UP),
        "down" => extended(VK_DOWN),
        "left" => extended(VK_LEFT),
        "right" => extended(VK_RIGHT),
        "space" => plain(VK_SPACE),
        "shift" => plain(VK_SHIFT),
        "control" => plain(VK_CONTROL),
        "alt" => plain(VK_MENU),
        "meta" => extended(VK_LWIN),
        "caps_lock" => plain(VK_CAPITAL),
        "print_screen" => extended(VK_SNAPSHOT),
        "menu" => extended(VK_APPS),
        _ => {
            let number: u16 = name.strip_prefix('f')?.parse().ok()?;
            if !(1..=24).contains(&number) {
                return None;
            }
            plain(VK_F1 + number - 1)
        }
    })
}

fn key_op(stroke: Stroke, up: bool) -> Op {
    let direction = if up { KEYEVENTF_KEYUP } else { 0 };
    match stroke {
        Stroke::Vk { code, extended } => Op::Key {
            vk: code,
            unit: 0,
            flags: direction | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 },
        },
        Stroke::Unit(unit) => Op::Key {
            vk: 0,
            unit,
            flags: direction | KEYEVENTF_UNICODE,
        },
    }
}

/// The operations of one key event: press, release, or both (the release in the reverse order).
fn key_ops(strokes: &[Stroke], action: KeyAction) -> Vec<Op> {
    let mut ops = Vec::new();
    if action != KeyAction::Release {
        ops.extend(strokes.iter().map(|stroke| key_op(*stroke, false)));
    }
    if action != KeyAction::Press {
        ops.extend(strokes.iter().rev().map(|stroke| key_op(*stroke, true)));
    }
    ops
}

/// The operations that type `text` as it is: a character as itself (UTF-16 units, so any layout), a line break as Enter
/// and a tab as Tab, which applications take for those keys and not for a character.
pub(crate) fn text_ops(text: &str) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        let strokes = match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                vec![plain(VK_RETURN)]
            }
            '\n' => vec![plain(VK_RETURN)],
            '\t' => vec![plain(VK_TAB)],
            other => {
                let mut units = [0_u16; 2];
                other
                    .encode_utf16(&mut units)
                    .iter()
                    .map(|unit| Stroke::Unit(*unit))
                    .collect()
            }
        };
        for stroke in strokes {
            ops.push(key_op(stroke, false));
            ops.push(key_op(stroke, true));
        }
    }
    ops
}

/// The pixel of the virtual screen a point of a display is.
fn on_virtual(
    state: &DesktopState,
    virtual_screen: Virtual,
    display: Option<&bohio_proto::gui::DisplayId>,
    x: i32,
    y: i32,
) -> Result<(i32, i32), GuiError> {
    let display = state.display(display)?;
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
    Ok(virtual_screen.normalise(display.x.saturating_add(x), display.y.saturating_add(y)))
}

fn move_op(point: (i32, i32)) -> Op {
    Op::Mouse {
        x: point.0,
        y: point.1,
        data: 0,
        flags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    }
}

fn button_flags(button: Button, up: bool) -> (u32, u32) {
    match (button, up) {
        (Button::Left, false) => (MOUSEEVENTF_LEFTDOWN, 0),
        (Button::Left, true) => (MOUSEEVENTF_LEFTUP, 0),
        (Button::Right, false) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (Button::Right, true) => (MOUSEEVENTF_RIGHTUP, 0),
        (Button::Middle, false) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (Button::Middle, true) => (MOUSEEVENTF_MIDDLEUP, 0),
        (Button::Back, false) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (Button::Back, true) => (MOUSEEVENTF_XUP, XBUTTON1),
        (Button::Forward, false) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (Button::Forward, true) => (MOUSEEVENTF_XUP, XBUTTON2),
    }
}

/// The operations of one event, in order. `wait` has none (the engine pauses itself).
///
/// # Errors
///
/// `bad_request` for a point outside its display or a key that is not one.
pub(crate) fn plan(
    event: &InputEvent,
    state: &DesktopState,
    virtual_screen: Virtual,
) -> Result<Vec<Op>, GuiError> {
    Ok(match event {
        InputEvent::Move { display, x, y } => {
            vec![move_op(on_virtual(
                state,
                virtual_screen,
                display.as_ref(),
                *x,
                *y,
            )?)]
        }
        InputEvent::Button {
            button,
            action,
            count,
            display,
            x,
            y,
        } => {
            let mut ops = Vec::new();
            if let (Some(x), Some(y)) = (x, y) {
                ops.push(move_op(on_virtual(
                    state,
                    virtual_screen,
                    display.as_ref(),
                    *x,
                    *y,
                )?));
            }
            let mouse = |up: bool| {
                let (flags, data) = button_flags(*button, up);
                Op::Mouse {
                    x: 0,
                    y: 0,
                    data,
                    flags,
                }
            };
            match action {
                ButtonAction::Press => ops.push(mouse(false)),
                ButtonAction::Release => ops.push(mouse(true)),
                ButtonAction::Click => {
                    for _ in 0..count.unwrap_or(1).max(1) {
                        ops.push(mouse(false));
                        ops.push(mouse(true));
                    }
                }
            }
            ops
        }
        InputEvent::Scroll {
            display,
            x,
            y,
            dx,
            dy,
        } => {
            let mut ops = Vec::new();
            if let (Some(x), Some(y)) = (x, y) {
                ops.push(move_op(on_virtual(
                    state,
                    virtual_screen,
                    display.as_ref(),
                    *x,
                    *y,
                )?));
            }
            // Windows' positive wheel is away from the person, which scrolls the content up: the wire's positive
            // `dy` scrolls it down. A positive horizontal wheel scrolls to the right, as the wire's `dx`.
            let wheel = |flags: u32, notches: i32| Op::Mouse {
                x: 0,
                y: 0,
                data: notches.saturating_mul(WHEEL_DELTA).cast_unsigned(),
                flags,
            };
            if *dy != 0 {
                ops.push(wheel(MOUSEEVENTF_WHEEL, dy.saturating_neg()));
            }
            if *dx != 0 {
                ops.push(wheel(MOUSEEVENTF_HWHEEL, *dx));
            }
            ops
        }
        InputEvent::Key { key, action } => {
            let strokes =
                strokes_of(key).ok_or_else(|| GuiError::bad_request("that is not a key name"))?;
            key_ops(&strokes, *action)
        }
        InputEvent::Text { text } => text_ops(text),
        InputEvent::Wait { .. } => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bohio_proto::gui::DisplayId;

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

    /// A main display and a second one to its left and above it, so its origin is negative.
    fn two() -> (DesktopState, Virtual) {
        let displays = vec![
            display("display1", true, 0, 0, 2560, 1440),
            display("display2", false, -1920, -300, 1920, 1080),
        ];
        let virtual_screen = Virtual::of(&displays).expect("two displays");
        (
            DesktopState {
                user: None,
                active: true,
                locked: Some(false),
                displays,
            },
            virtual_screen,
        )
    }

    #[test]
    fn the_virtual_screen_spans_every_display() {
        let (_, virtual_screen) = two();
        assert_eq!(
            virtual_screen,
            Virtual {
                x: -1920,
                y: -300,
                width: 1920 + 2560,
                height: 1440 + 300,
            }
        );
        assert_eq!(Virtual::of(&[]), None);
    }

    #[test]
    fn every_pixel_of_the_virtual_screen_comes_back_to_itself() {
        let (_, virtual_screen) = two();
        for y in (virtual_screen.y..virtual_screen.y + virtual_screen.height as i32).step_by(7) {
            for x in virtual_screen.x..virtual_screen.x + virtual_screen.width as i32 {
                let (nx, ny) = virtual_screen.normalise(x, y);
                assert!((0..=65535).contains(&nx) && (0..=65535).contains(&ny));
                assert_eq!(virtual_screen.denormalise(nx, ny), (x, y));
            }
        }
    }

    #[test]
    fn a_point_on_the_second_display_is_not_the_main_displays_point() {
        let (state, virtual_screen) = two();
        // The second display's top-left corner is the virtual screen's: absolute 0, 0. Normalised to the main display
        // alone (the mistake this guards against) its own x and y would be negative and clamp to the same corner for
        // every point left of the main display.
        let ops = plan(
            &InputEvent::Move {
                display: Some(DisplayId("display2".to_owned())),
                x: 0,
                y: 0,
            },
            &state,
            virtual_screen,
        )
        .unwrap();
        assert_eq!(ops.len(), 1);
        let Op::Mouse { x, y, flags, .. } = ops[0] else {
            panic!("a mouse operation");
        };
        assert_eq!((x, y), (0, 0));
        assert_eq!(
            flags,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
        );
        let second = plan(
            &InputEvent::Move {
                display: Some(DisplayId("display2".to_owned())),
                x: 1000,
                y: 500,
            },
            &state,
            virtual_screen,
        )
        .unwrap();
        let main = plan(
            &InputEvent::Move {
                display: None,
                x: 1000,
                y: 500,
            },
            &state,
            virtual_screen,
        )
        .unwrap();
        let (Op::Mouse { x: sx, y: sy, .. }, Op::Mouse { x: mx, y: my, .. }) = (second[0], main[0])
        else {
            panic!("mouse operations");
        };
        assert_eq!(virtual_screen.denormalise(sx, sy), (-920, 200));
        assert_eq!(virtual_screen.denormalise(mx, my), (1000, 500));
    }

    #[test]
    fn a_point_outside_its_display_is_refused_naming_the_size() {
        let (state, virtual_screen) = two();
        for (x, y) in [(-1, 0), (0, -1), (1920, 0), (0, 1080), (i32::MAX, 0)] {
            let error = plan(
                &InputEvent::Move {
                    display: Some(DisplayId("display2".to_owned())),
                    x,
                    y,
                },
                &state,
                virtual_screen,
            )
            .unwrap_err();
            assert_eq!(error.code, bohio_proto::ErrorCode::BadRequest);
            assert!(error.message.contains("1920 by 1080"), "{error}");
        }
    }

    #[test]
    fn a_click_moves_first_and_presses_then_releases_count_times() {
        let (state, virtual_screen) = two();
        let ops = plan(
            &InputEvent::Button {
                button: Button::Right,
                action: ButtonAction::Click,
                count: Some(2),
                display: None,
                x: Some(10),
                y: Some(20),
            },
            &state,
            virtual_screen,
        )
        .unwrap();
        let flags: Vec<u32> = ops
            .iter()
            .map(|op| match op {
                Op::Mouse { flags, .. } => *flags,
                Op::Key { .. } => panic!("a mouse operation"),
            })
            .collect();
        assert_eq!(
            flags,
            [
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                MOUSEEVENTF_RIGHTDOWN,
                MOUSEEVENTF_RIGHTUP,
                MOUSEEVENTF_RIGHTDOWN,
                MOUSEEVENTF_RIGHTUP,
            ]
        );
    }

    #[test]
    fn back_and_forward_are_the_extra_buttons() {
        assert_eq!(
            button_flags(Button::Back, false),
            (MOUSEEVENTF_XDOWN, XBUTTON1)
        );
        assert_eq!(
            button_flags(Button::Forward, true),
            (MOUSEEVENTF_XUP, XBUTTON2)
        );
    }

    #[test]
    fn the_wheel_is_120_a_notch_and_positive_dy_scrolls_down() {
        let (state, virtual_screen) = two();
        let ops = plan(
            &InputEvent::Scroll {
                display: None,
                x: None,
                y: None,
                dx: -2,
                dy: 3,
            },
            &state,
            virtual_screen,
        )
        .unwrap();
        assert_eq!(
            ops,
            [
                Op::Mouse {
                    x: 0,
                    y: 0,
                    data: (-360_i32).cast_unsigned(),
                    flags: MOUSEEVENTF_WHEEL,
                },
                Op::Mouse {
                    x: 0,
                    y: 0,
                    data: (-240_i32).cast_unsigned(),
                    flags: MOUSEEVENTF_HWHEEL,
                },
            ]
        );
        // The most a request can ask for does not overflow.
        let huge = plan(
            &InputEvent::Scroll {
                display: None,
                x: None,
                y: None,
                dx: 0,
                dy: i32::MIN,
            },
            &state,
            virtual_screen,
        )
        .unwrap();
        assert_eq!(huge.len(), 1);
    }

    #[test]
    fn every_name_of_the_wire_is_a_key() {
        for name in bohio_proto::gui::KEY_NAMES {
            assert!(strokes_of(name).is_some(), "{name} has no virtual key");
        }
        assert_eq!(strokes_of("f13"), Some(vec![plain(0x7C)]));
        assert_eq!(strokes_of("f24"), Some(vec![plain(0x87)]));
        assert_eq!(strokes_of("f25"), None);
        assert_eq!(strokes_of("f0"), None);
        assert_eq!(strokes_of("nonsense"), None);
        assert_eq!(strokes_of(""), None);
    }

    #[test]
    fn keys_that_need_the_extended_bit_have_it() {
        for name in [
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
            "meta",
            "print_screen",
            "menu",
        ] {
            assert!(
                matches!(
                    strokes_of(name).unwrap()[0],
                    Stroke::Vk { extended: true, .. }
                ),
                "{name}"
            );
        }
        for name in [
            "enter",
            "tab",
            "escape",
            "backspace",
            "space",
            "shift",
            "control",
            "alt",
            "f5",
        ] {
            assert!(
                matches!(
                    strokes_of(name).unwrap()[0],
                    Stroke::Vk {
                        extended: false,
                        ..
                    }
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn a_letter_or_digit_is_its_key_whatever_its_case_and_any_other_character_is_typed() {
        assert_eq!(strokes_of("s"), Some(vec![plain(0x53)]));
        assert_eq!(strokes_of("S"), Some(vec![plain(0x53)]));
        assert_eq!(strokes_of("7"), Some(vec![plain(0x37)]));
        assert_eq!(strokes_of(" "), Some(vec![plain(VK_SPACE)]));
        assert_eq!(strokes_of("é"), Some(vec![Stroke::Unit(0x00E9)]));
        assert_eq!(strokes_of("/"), Some(vec![Stroke::Unit(0x002F)]));
        // Outside the basic plane: a surrogate pair.
        assert_eq!(
            strokes_of("😀"),
            Some(vec![Stroke::Unit(0xD83D), Stroke::Unit(0xDE00)])
        );
    }

    #[test]
    fn a_combo_presses_in_order_and_releases_in_reverse() {
        let (state, virtual_screen) = two();
        let key = |key: &str, action| InputEvent::Key {
            key: key.to_owned(),
            action,
        };
        let mut ops = Vec::new();
        for event in [
            key("control", KeyAction::Press),
            key("s", KeyAction::Click),
            key("control", KeyAction::Release),
        ] {
            ops.extend(plan(&event, &state, virtual_screen).unwrap());
        }
        assert_eq!(
            ops,
            [
                Op::Key {
                    vk: 0x11,
                    unit: 0,
                    flags: 0
                },
                Op::Key {
                    vk: 0x53,
                    unit: 0,
                    flags: 0
                },
                Op::Key {
                    vk: 0x53,
                    unit: 0,
                    flags: KEYEVENTF_KEYUP
                },
                Op::Key {
                    vk: 0x11,
                    unit: 0,
                    flags: KEYEVENTF_KEYUP
                },
            ]
        );
    }

    #[test]
    fn text_is_typed_as_units_with_line_breaks_as_enter() {
        let ops = text_ops("a\r\né\t");
        let expected = [
            (0, 0x61, KEYEVENTF_UNICODE),
            (0, 0x61, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
            (VK_RETURN, 0, 0),
            (VK_RETURN, 0, KEYEVENTF_KEYUP),
            (0, 0xE9, KEYEVENTF_UNICODE),
            (0, 0xE9, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
            (VK_TAB, 0, 0),
            (VK_TAB, 0, KEYEVENTF_KEYUP),
        ]
        .map(|(vk, unit, flags)| Op::Key { vk, unit, flags });
        assert_eq!(ops, expected);
        assert!(text_ops("").is_empty());
    }
}
