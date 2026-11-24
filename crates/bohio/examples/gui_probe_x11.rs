//! A window that paints a known pattern and says what it receives, for `tests/gui_x11.rs`.
//!
//! `gui_probe_x11 DISPLAY [X Y]` makes a 128 by 96 window (at X, Y in the root when given: a display with no window
//! manager keeps it there), paints the pattern of `pattern`, and prints one line each:
//!
//! - `ready X Y W H ID`: once painted, and again whenever it moves, where the window's pixels are in the root, and its
//!   window id;
//! - `focused` and `unfocused`, as the keyboard focus comes and goes;
//! - `key U+XXXX` (a character) or `key keysym:XXXX` for each key press, from the keyboard mapping as it is then;
//! - `button N X Y`: a button press and the root point it came at.
//!
//! It ends when its standard input closes. It draws nothing outside its own window and reads nothing but its events.

#[path = "support/x11_pattern.rs"]
mod pattern;

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn main() {
    probe::run();
}

#[cfg(target_os = "linux")]
mod probe {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use x11rb::COPY_DEPTH_FROM_PARENT;
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{
        AtomEnum, ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask,
        InputFocus, PropMode, Rectangle, VisualClass, WindowClass,
    };
    use x11rb::wrapper::ConnectionExt as _;
    use xkeysym::Keysym;

    use super::pattern::{BLOCK, HEIGHT, PALETTE, WIDTH, colour};

    /// Says where the window's pixels are in the root, again each time that changes (a compositor may place a window after
    /// it is shown).
    fn announce(
        connection: &impl Connection,
        window: u32,
        root: u32,
        last: &mut Option<(i16, i16)>,
    ) {
        let at = connection
            .translate_coordinates(window, root, 0, 0)
            .expect("translate")
            .reply()
            .expect("a position");
        if *last != Some((at.dst_x, at.dst_y)) {
            *last = Some((at.dst_x, at.dst_y));
            say(&format!(
                "ready {} {} {WIDTH} {HEIGHT} {window}",
                at.dst_x, at.dst_y
            ));
        }
    }

    fn say(line: &str) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }

    pub fn run() {
        let mut args = std::env::args().skip(1);
        let display = args.next();
        let place = match (args.next(), args.next()) {
            (Some(x), Some(y)) => (x.parse::<i16>().unwrap_or(0), y.parse::<i16>().unwrap_or(0)),
            _ => (0, 0),
        };
        let (connection, number) =
            x11rb::rust_connection::RustConnection::connect(display.as_deref()).expect("a display");
        let setup = connection.setup();
        let screen = &setup.roots[number];
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| &depth.visuals)
            .find(|visual| visual.visual_id == screen.root_visual)
            .expect("the root visual");
        assert!(
            visual.class == VisualClass::TRUE_COLOR || visual.class == VisualClass::DIRECT_COLOR,
            "a true colour display"
        );
        let pixel = |(r, g, b): (u8, u8, u8)| -> u32 {
            let part = |value: u8, mask: u32| -> u32 {
                let shift = mask.trailing_zeros();
                let max = mask >> shift;
                (u32::from(value) * max / 255) << shift
            };
            part(r, visual.red_mask) | part(g, visual.green_mask) | part(b, visual.blue_mask)
        };

        let window = connection.generate_id().expect("an id");
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                screen.root,
                place.0,
                place.1,
                WIDTH,
                HEIGHT,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().event_mask(
                    EventMask::EXPOSURE
                        | EventMask::KEY_PRESS
                        | EventMask::BUTTON_PRESS
                        | EventMask::FOCUS_CHANGE
                        | EventMask::STRUCTURE_NOTIFY,
                ),
            )
            .expect("a window");
        connection
            .change_property8(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                b"bohio probe",
            )
            .expect("a title");
        let gc = connection.generate_id().expect("an id");
        connection
            .create_gc(gc, window, &CreateGCAux::new())
            .expect("a gc");
        connection.map_window(window).expect("map");
        connection
            .set_input_focus(InputFocus::POINTER_ROOT, window, x11rb::CURRENT_TIME)
            .expect("focus");
        connection.flush().expect("flush");

        let closed = Arc::new(AtomicBool::new(false));
        {
            let closed = Arc::clone(&closed);
            std::thread::spawn(move || {
                let mut sink = [0_u8; 64];
                let mut input = std::io::stdin().lock();
                while input.read(&mut sink).is_ok_and(|n| n > 0) {}
                closed.store(true, Ordering::Release);
            });
        }

        let mut painted = false;
        let mut last = None;
        while !closed.load(Ordering::Acquire) {
            let Some(event) = connection.poll_for_event().expect("events") else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            match event {
                Event::Expose(expose) if expose.count == 0 => {
                    for colour_of in &PALETTE {
                        let mut rectangles = Vec::new();
                        for by in 0..HEIGHT.div_ceil(BLOCK) {
                            for bx in 0..WIDTH.div_ceil(BLOCK) {
                                if colour(bx * BLOCK, by * BLOCK) == *colour_of {
                                    rectangles.push(Rectangle {
                                        x: i16::try_from(bx * BLOCK).unwrap_or(0),
                                        y: i16::try_from(by * BLOCK).unwrap_or(0),
                                        width: BLOCK,
                                        height: BLOCK,
                                    });
                                }
                            }
                        }
                        connection
                            .change_gc(gc, &ChangeGCAux::new().foreground(pixel(*colour_of)))
                            .expect("a colour");
                        connection
                            .poly_fill_rectangle(window, gc, &rectangles)
                            .expect("paint");
                    }
                    connection.flush().expect("flush");
                    painted = true;
                    announce(&connection, window, screen.root, &mut last);
                }
                Event::ConfigureNotify(_) if painted => {
                    announce(&connection, window, screen.root, &mut last);
                }
                Event::FocusIn(_) => say("focused"),
                Event::FocusOut(_) => say("unfocused"),
                Event::KeyPress(press) => {
                    let mapping = connection
                        .get_keyboard_mapping(press.detail, 1)
                        .expect("a mapping")
                        .reply()
                        .expect("a mapping");
                    let shifted = u16::from(press.state) & 1 != 0;
                    let column = usize::from(shifted && mapping.keysyms.len() > 1);
                    let sym = mapping.keysyms.get(column).copied().unwrap_or(0);
                    match Keysym::new(sym).key_char() {
                        Some(character) => say(&format!("key U+{:04X}", u32::from(character))),
                        None => say(&format!("key keysym:{sym:X}")),
                    }
                }
                Event::ButtonPress(press) => {
                    say(&format!(
                        "button {} {} {}",
                        press.detail, press.root_x, press.root_y
                    ));
                }
                Event::DestroyNotify(_) => break,
                _ => {}
            }
        }
    }
}
