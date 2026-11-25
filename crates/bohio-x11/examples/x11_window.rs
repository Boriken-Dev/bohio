// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A window of one colour on an X display, for tests of what a desktop shows and of forwarding.
//!
//! `x11_window [--seconds N] [--x X --y Y] [--size W H] [--rgb RRGGBB]`: connects to `DISPLAY` (with the cookie in
//! `X11_COOKIE` as hex when it is set, for a display reached over a forwarded port), maps the window, prints `mapped`,
//! then prints `click X Y` for each button press (window coordinates) and `ended` when the time is up.

use std::io::Write as _;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use x11rb::COPY_DEPTH_FROM_PARENT;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, EventMask, WindowClass};
use x11rb::rust_connection::{DefaultStream, RustConnection};

fn say(text: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

fn argument(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|at| args.get(at + 1).cloned())
}

fn connect() -> Result<(RustConnection, usize), String> {
    let display = std::env::var("DISPLAY").map_err(|_| "DISPLAY is not set".to_owned())?;
    let cookie = std::env::var("X11_COOKIE")
        .ok()
        .filter(|text| !text.is_empty());
    let (Some(cookie), Some((host, rest))) = (cookie, display.rsplit_once(':')) else {
        return RustConnection::connect(Some(&display)).map_err(|error| error.to_string());
    };
    let number: u16 = rest
        .split('.')
        .next()
        .and_then(|text| text.parse().ok())
        .ok_or("DISPLAY has no number")?;
    let data: Vec<u8> = (0..cookie.len() / 2)
        .filter_map(|at| u8::from_str_radix(cookie.get(2 * at..2 * at + 2)?, 16).ok())
        .collect();
    let tcp = TcpStream::connect((host, 6000 + number)).map_err(|error| error.to_string())?;
    let (stream, _) = DefaultStream::from_tcp_stream(tcp).map_err(|error| error.to_string())?;
    RustConnection::connect_to_stream_with_auth_info(
        stream,
        0,
        b"MIT-MAGIC-COOKIE-1".to_vec(),
        data,
    )
    .map(|connection| (connection, 0))
    .map_err(|error| error.to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let number = |name: &str, default: i16| {
        argument(&args, name)
            .and_then(|text| text.parse().ok())
            .unwrap_or(default)
    };
    let seconds = argument(&args, "--seconds")
        .and_then(|text| text.parse().ok())
        .unwrap_or(30_u64);
    let (width, height) = args
        .iter()
        .position(|arg| arg == "--size")
        .and_then(|at| {
            Some((
                args.get(at + 1)?.parse().ok()?,
                args.get(at + 2)?.parse().ok()?,
            ))
        })
        .unwrap_or((240_u16, 160_u16));
    let colour = argument(&args, "--rgb")
        .and_then(|text| u32::from_str_radix(&text, 16).ok())
        .unwrap_or(0x00FF_0000);
    let (connection, screen) = match connect() {
        Ok(made) => made,
        Err(why) => {
            eprintln!("cannot connect: {why}");
            std::process::exit(2);
        }
    };
    let root = connection.setup().roots[screen].root;
    let window = connection.generate_id().expect("an id");
    connection
        .create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            number("--x", 40),
            number("--y", 40),
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(colour)
                .event_mask(EventMask::BUTTON_PRESS | EventMask::EXPOSURE),
        )
        .expect("a window");
    connection.map_window(window).expect("mapped");
    connection.flush().expect("flushed");
    say("mapped");
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        match connection.poll_for_event() {
            Ok(Some(Event::ButtonPress(press))) => {
                say(&format!("click {} {}", press.event_x, press.event_y));
            }
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    say("ended");
}
