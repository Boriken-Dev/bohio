//! The macOS desktop backend against the screen it runs in.
//!
//! The tests that touch nothing run wherever the suite does: with no login session or without the grants of System
//! Settings (an ssh login, a runner with no Screen Recording or Accessibility grant) they prove that each refusal is
//! `unavailable` and names the pane and the program's path; with both present they prove nothing of that and say so.
//!
//! The live test captures and drives the desktop only when `BOHIO_TEST_GUI_LIVE=1`: it makes its own window
//! (`support/gui_probe.swift`: a known pattern and a text field), captures only that window's region, clicks, types and
//! scrolls only on it (and only while it reports that it is the key window), puts the pointer and the clipboard's text back
//! as it found them, and ends the window. Without a session, or without a grant, it prints `SKIPPED` with the system's own
//! reason. No image is written anywhere.
//!
//! The grants belong to the program at its path: Screen Recording and Accessibility must be given, in System Settings, to
//! the test binary `cargo test` prints (`target/debug/deps/gui_macos-...`), and to the terminal that starts it when that
//! is what the system asks for.

#![cfg(target_os = "macos")]
#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use bohio::gui::{Desktop, GuiError, MacosDesktop, MacosSettings};
use bohio_proto::ErrorCode;
use bohio_proto::gui::{Button, ButtonAction, DisplayId, InputEvent, KeyAction, Rect};

const WAIT: Duration = Duration::from_secs(30);

fn live(test: &str) -> bool {
    if std::env::var("BOHIO_TEST_GUI_LIVE").as_deref() == Ok("1") {
        return true;
    }
    println!(
        "SKIPPED: {test}: BOHIO_TEST_GUI_LIVE=1 is not set, so nothing of the desktop is captured or driven"
    );
    false
}

fn desktop() -> MacosDesktop {
    MacosDesktop::new(MacosSettings::default())
}

/// What a refusal for a missing grant must say: the pane, and the program the grant is for.
fn assert_names_the_grant(error: &GuiError, pane: &str) {
    assert_eq!(error.code, ErrorCode::Unavailable, "{error}");
    let binary = std::env::current_exe().expect("the test's own path");
    for word in [
        "System Settings",
        "Privacy & Security",
        pane,
        binary.to_str().expect("a UTF-8 path"),
    ] {
        assert!(error.message.contains(word), "{word}: {error}");
    }
}

#[test]
fn the_backend_says_what_it_can_do() {
    let capabilities = desktop().capabilities();
    assert_eq!(capabilities.backend, "macos");
    assert!(capabilities.clipboard && capabilities.lock_detection);
}

#[test]
fn with_no_session_the_state_is_unavailable_and_says_how_to_get_one() {
    match desktop().state() {
        Ok(state) => {
            assert!(!state.displays.is_empty());
            assert_eq!(state.displays.iter().filter(|d| d.primary).count(), 1);
            for display in &state.displays {
                assert!(display.width > 0 && display.height > 0);
                assert!(display.scale >= 1.0, "{display:?}");
            }
            assert!(state.locked.is_some());
            println!(
                "SESSION: this process has a login session; the no-session refusal was not exercised"
            );
        }
        Err(error) => {
            assert_eq!(error.code, ErrorCode::Unavailable, "{error}");
            assert!(
                error.message.contains("logged in") || error.message.contains("no display"),
                "{error}"
            );
            println!("NO SESSION: {error}");
        }
    }
}

#[test]
fn without_screen_recording_a_capture_is_unavailable_and_names_the_pane_and_the_program() {
    let desktop = desktop();
    // The grant is asked before the session and before the display, so a display that may not exist still meets it.
    let display = DisplayId("display1".to_owned());
    match desktop.capture(&display, Some(Rect::new(0, 0, 1, 1))) {
        // One pixel, dropped at once; with the grant this is no proof of the refusal.
        Ok(_) => println!("GRANTED: Screen Recording is allowed; the refusal was not exercised"),
        Err(error) if error.message.contains("Screen Recording") => {
            assert_names_the_grant(&error, "Screen Recording");
            println!("REFUSED: {error}");
        }
        Err(error) => {
            // The grant is there but no session, no such display, or macOS before 14: never `internal`.
            assert_ne!(error.code, ErrorCode::Internal, "{error}");
            println!("GRANTED, NOT CAPTURED: {error}");
        }
    }
}

#[test]
fn without_accessibility_input_is_unavailable_and_names_the_pane_and_the_program() {
    let desktop = desktop();
    if desktop.capabilities().input {
        println!(
            "GRANTED: Accessibility is allowed; the refusal was not exercised, and no event is sent"
        );
        return;
    }
    // A wait plans no step, so nothing is posted even if the system changed its mind between the two calls.
    let outcome = desktop.input(&[InputEvent::Wait { ms: 0 }]);
    match outcome {
        Err(error) if error.message.contains("Accessibility") => {
            assert_names_the_grant(&error, "Accessibility");
            println!("REFUSED: {error}");
        }
        Err(error) => {
            // The grant is there but no session: the state is asked next.
            assert_eq!(error.code, ErrorCode::Unavailable, "{error}");
            println!("GRANTED, NO SESSION: {error}");
        }
        Ok(applied) => println!("GRANTED between the calls: {applied} events applied"),
    }
}

#[test]
fn a_batch_with_a_point_off_its_display_is_refused_whole_and_nothing_is_posted() {
    let desktop = desktop();
    let Ok(state) = desktop.state() else {
        println!("NO SESSION: the batch refusal was not exercised");
        return;
    };
    let primary = state.display(None).expect("a display").clone();
    // The second event names a pixel past the display: the whole batch is refused before the first is posted.
    let error = desktop
        .input(&[
            InputEvent::Move {
                display: None,
                x: 5,
                y: 5,
            },
            InputEvent::Move {
                display: None,
                x: i32::try_from(primary.width).unwrap(),
                y: 0,
            },
        ])
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest, "{error}");
    assert!(error.message.contains("outside the display"), "{error}");
}

#[test]
fn a_clipboard_text_over_the_limit_is_refused_before_the_pasteboard_is_opened() {
    let desktop = MacosDesktop::new(MacosSettings {
        clipboard_max_bytes: 8,
        ..MacosSettings::default()
    });
    let error = desktop.set_clipboard_text("123456789").unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest);
}

/// The colour of the probe's cell `cx`, `cy`, as `gui_probe.swift` paints it.
fn cell(cx: u32, cy: u32) -> [u8; 3] {
    [
        u8::try_from(30 + 50 * cx).unwrap(),
        u8::try_from(30 + 50 * cy).unwrap(),
        u8::try_from(60 + 30 * ((cx + cy) % 4)).unwrap(),
    ]
}

/// The probe window: its output, a line at a time.
struct Probe {
    child: Child,
    lines: Receiver<String>,
    focused: bool,
    seen: Vec<String>,
}

impl Probe {
    fn start(x: i32, y: i32) -> Self {
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/gui_probe.swift");
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gui_probe_mac");
        let built = Command::new("xcrun")
            .arg("swiftc")
            .arg(&source)
            .arg("-o")
            .arg(&out)
            .output()
            .expect("run xcrun swiftc");
        assert!(
            built.status.success(),
            "the probe did not build: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        let mut child = Command::new(&out)
            .args([&x.to_string(), &y.to_string(), "120"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the probe");
        let output = child.stdout.take().expect("probe output");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            lines,
            focused: false,
            seen: Vec::new(),
        }
    }

    /// The next line within `wait`, with the focus state it implies kept.
    fn next(&mut self, wait: Duration) -> Option<String> {
        match self.lines.recv_timeout(wait) {
            Ok(line) => {
                match line.as_str() {
                    "focused" => self.focused = true,
                    "unfocused" => self.focused = false,
                    _ => {}
                }
                self.seen.push(line.clone());
                Some(line)
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Reads until a line that `matches` arrives; the lines before it are returned with it.
    fn until(&mut self, what: &str, matches: impl Fn(&str) -> bool) -> Vec<String> {
        let deadline = Instant::now() + WAIT;
        let mut read = Vec::new();
        while let Some(line) = self.next(deadline.saturating_duration_since(Instant::now())) {
            let done = matches(&line);
            read.push(line);
            if done {
                return read;
            }
        }
        panic!("the probe never reported {what}; it said {:?}", self.seen);
    }

    /// Takes what the probe has said so far, without waiting.
    fn drain(&mut self) -> Vec<String> {
        let mut read = Vec::new();
        while let Some(line) = self.next(Duration::from_millis(1)) {
            read.push(line);
        }
        read
    }

    /// Lets the probe's events arrive, then says whether it is the key window.
    fn has_focus(&mut self) -> bool {
        let _ = self.next(Duration::from_millis(150));
        self.drain();
        self.focused
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Puts the pointer back where it was when the test started, even when it fails.
struct PutPointerBack<'a> {
    desktop: &'a MacosDesktop,
    at: Option<(DisplayId, i32, i32)>,
}

impl Drop for PutPointerBack<'_> {
    fn drop(&mut self) {
        if let Some((display, x, y)) = self.at.take() {
            let _ = self.desktop.input(&[InputEvent::Move {
                display: Some(display),
                x,
                y,
            }]);
        }
    }
}

/// Puts the clipboard's text back, even when the test fails.
struct PutClipboardBack<'a> {
    desktop: &'a MacosDesktop,
    text: Option<String>,
}

impl Drop for PutClipboardBack<'_> {
    fn drop(&mut self) {
        if let Some(text) = self.text.take() {
            let _ = self.desktop.set_clipboard_text(&text);
        }
    }
}

fn click(display: &DisplayId, x: i32, y: i32, button: Button) -> InputEvent {
    InputEvent::Button {
        button,
        action: ButtonAction::Click,
        count: None,
        display: Some(display.clone()),
        x: Some(x),
        y: Some(y),
    }
}

fn key(key: &str, action: KeyAction) -> InputEvent {
    InputEvent::Key {
        key: key.to_owned(),
        action,
    }
}

/// The text the probe's field holds after the last `text` line it said.
fn last_text(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix("text"))
        .map(|escaped| {
            let mut bytes = Vec::new();
            let mut chars = escaped.trim_start().bytes();
            while let Some(byte) = chars.next() {
                if byte == b'%' {
                    let digits = [chars.next().unwrap(), chars.next().unwrap()];
                    bytes.push(
                        u8::from_str_radix(std::str::from_utf8(&digits).unwrap(), 16).unwrap(),
                    );
                } else {
                    bytes.push(byte);
                }
            }
            String::from_utf8(bytes).unwrap()
        })
}

/// One test, in order: they share the screen, the pointer, the keyboard and the clipboard.
#[test]
fn the_probe_window_is_captured_and_driven() {
    let name = "the_probe_window_is_captured_and_driven";
    if !live(name) {
        return;
    }
    let desktop = desktop();
    let state = match desktop.state() {
        Ok(state) if state.active && state.locked == Some(false) => state,
        other => {
            println!("SKIPPED: {name}: no unlocked console session ({other:?})");
            return;
        }
    };
    let primary = state.display(None).expect("a display").clone();
    // The first capture is the permission check: a refusal is the system's own, with the grant to give.
    if let Err(error) = desktop.capture(&primary.id, Some(Rect::new(0, 0, 4, 4))) {
        println!("SKIPPED: {name}: the screen cannot be captured ({error})");
        return;
    }
    let input = desktop.capabilities().input;
    let back = PutPointerBack {
        desktop: &desktop,
        at: input.then(|| desktop.cursor()).flatten(),
    };
    let scale = primary.scale;
    // The probe sits a little inside the primary display, in points of the global space.
    let (x, y) = (primary.x + 120, primary.y + 120);
    let mut probe = Probe::start(x, y);
    let ready = probe.until("ready", |line| line.starts_with("ready "));
    let said: Vec<&str> = ready.last().unwrap().split_whitespace().collect();
    assert_eq!(
        &said[1..5],
        [&x.to_string(), &y.to_string(), "128", "128"],
        "{said:?}"
    );
    let backing: f64 = said[5].parse().unwrap();
    assert!(
        (backing - scale).abs() < 0.01,
        "the probe's scale {backing} is not the display's {scale}"
    );

    // Capture: the middle of each cell of the probe's pattern is the colour it painted.
    let side = 128.0 * scale;
    let origin = |points: i32| f64::from(points) * scale;
    let region = Rect::new(
        origin(x - primary.x) as u32,
        origin(y - primary.y) as u32,
        side as u32,
        side as u32,
    );
    let wanted = |cx: u32, cy: u32| cell(cx, cy);
    let deadline = Instant::now() + Duration::from_secs(10);
    let frame = loop {
        let frame = desktop
            .capture(&primary.id, Some(region))
            .unwrap_or_else(|error| panic!("the probe was not captured: {error}"));
        let width = frame.width() as usize;
        let at = |px: usize, py: usize| &frame.rgb()[(py * width + px) * 3..][..3];
        let half = (32.0 * scale) as usize;
        let matches = (0..4_u32).all(|cy| {
            (0..4_u32).all(|cx| {
                let got = at(cx as usize * half + half / 2, cy as usize * half + half / 2);
                got.iter()
                    .zip(wanted(cx, cy))
                    .all(|(got, want)| got.abs_diff(want) <= 4)
            })
        });
        if matches || Instant::now() > deadline {
            break (matches, frame);
        }
        thread::sleep(Duration::from_millis(100));
    };
    assert!(
        frame.0,
        "the probe's pattern was not what the capture holds"
    );
    println!(
        "capture: the middle of every cell of the probe's 4x4 pattern is its colour (within 4)"
    );

    if !input {
        println!("SKIPPED: input of {name}: Accessibility is not allowed for this program");
        drop(back);
        return;
    }

    // Clicks land on the pixel aimed at: the probe reports the point of the global space.
    let (left, top) = (primary.x, primary.y);
    let on = |dx: i32, dy: i32| {
        (
            origin(x - left + dx).round() as i32,
            origin(y - top + dy).round() as i32,
        )
    };
    for (dx, dy) in [(0, 0), (127, 127), (64, 64), (10, 100)] {
        let _ = probe.drain();
        let (px, py) = on(dx, dy);
        let applied = desktop
            .input(&[click(&primary.id, px, py, Button::Left)])
            .expect("a click");
        assert_eq!(applied, 1);
        let lines = probe.until("the click", |line| line.starts_with("mouse "));
        assert_eq!(
            lines.last().unwrap(),
            &format!("mouse left {} {}", x + dx, y + dy),
            "a click at ({dx}, {dy}) of the probe landed elsewhere"
        );
    }
    println!("click: four points of the probe, each at the point that holds the pixel aimed at");

    // Keyboard: only while the probe says it is the key window.
    let _ = probe.drain();
    if !probe.has_focus() {
        let (px, py) = on(64, 64);
        desktop
            .input(&[click(&primary.id, px, py, Button::Left)])
            .unwrap();
        thread::sleep(Duration::from_millis(300));
    }
    assert!(
        probe.has_focus(),
        "the probe is not the key window, so nothing is typed; it said {:?}",
        probe.seen
    );
    let _ = probe.drain();
    desktop
        .input(&[InputEvent::Text {
            text: "abc é".to_owned(),
        }])
        .unwrap();
    thread::sleep(Duration::from_millis(400));
    let lines = probe.drain();
    assert!(
        probe.focused,
        "the probe lost the focus while it was typed to: {lines:?}"
    );
    assert_eq!(last_text(&lines).as_deref(), Some("abc é"), "{lines:?}");
    println!("text: ASCII and a non-ASCII character reached the probe's field in order");

    // A combo: command+a selects all, backspace removes it.
    assert!(probe.has_focus(), "the probe lost the focus");
    desktop
        .input(&[
            key("meta", KeyAction::Press),
            key("a", KeyAction::Click),
            key("meta", KeyAction::Release),
            key("backspace", KeyAction::Click),
        ])
        .unwrap();
    thread::sleep(Duration::from_millis(400));
    let lines = probe.drain();
    assert_eq!(last_text(&lines).as_deref(), Some(""), "{lines:?}");
    println!("combo: command+a then backspace emptied the probe's field");

    // The wheel: two lines down. The sign the probe reports depends on the person's natural-scrolling setting, so only
    // its size is asserted; the sign is printed for the record.
    let (px, py) = on(64, 64);
    desktop
        .input(&[
            InputEvent::Move {
                display: Some(primary.id.clone()),
                x: px,
                y: py,
            },
            InputEvent::Scroll {
                display: None,
                x: None,
                y: None,
                dx: 0,
                dy: 2,
            },
        ])
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    let lines = probe.drain();
    let wheel = lines
        .iter()
        .find(|line| line.starts_with("wheel V "))
        .unwrap_or_else(|| panic!("no wheel event arrived: {lines:?}"));
    let vertical: f64 = wheel.split_whitespace().nth(2).unwrap().parse().unwrap();
    assert!((vertical.abs() - 2.0).abs() < 0.5, "{wheel}");
    println!("scroll: two lines down arrived as {wheel}");

    // The pasteboard's text, put back after.
    match desktop.clipboard_text() {
        Ok(original) => {
            let restore = PutClipboardBack {
                desktop: &desktop,
                text: Some(original),
            };
            let text = "h\u{e9}llo\nw\u{f6}rld \u{1f600}";
            desktop.set_clipboard_text(text).unwrap();
            assert_eq!(desktop.clipboard_text().unwrap(), text);
            drop(restore);
            println!("clipboard: a text with a line break and an astral character round-trips");
        }
        Err(GuiError { message, .. }) => {
            println!(
                "SKIPPED: clipboard round trip: the pasteboard holds no text ({message}), and a test must not replace it"
            );
        }
    }
    drop(back);
}
