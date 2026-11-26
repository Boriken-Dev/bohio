//! The Windows desktop backend against the screen it runs in.
//!
//! Nothing here captures or drives the desktop unless `BOHIO_TEST_GUI_LIVE=1`; without it the live tests print `SKIPPED`
//! with the reason and touch nothing. With it, the one live test makes its own window (`support/gui_probe.ps1`: a known
//! pattern and a text box), captures only that window's region, clicks, types and scrolls only on it (and only while it
//! reports that it has the focus), puts the pointer and the clipboard back as it found them, and ends the window. No
//! image is written anywhere.

#![cfg(windows)]
#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use bohio::config::WindowsCapture;
use bohio::gui::{Desktop, GuiError, WindowsDesktop, WindowsSettings};
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

fn desktop(capture: WindowsCapture) -> WindowsDesktop {
    WindowsDesktop::new(WindowsSettings {
        capture,
        ..WindowsSettings::default()
    })
}

#[test]
fn the_state_names_the_displays_and_the_session() {
    let desktop = desktop(WindowsCapture::Auto);
    let capabilities = desktop.capabilities();
    assert_eq!(capabilities.backend, "windows");
    assert!(capabilities.input && capabilities.clipboard && capabilities.lock_detection);
    match desktop.state() {
        Ok(state) => {
            assert!(!state.displays.is_empty());
            assert_eq!(state.displays.iter().filter(|d| d.primary).count(), 1);
            let mut ids: Vec<_> = state.displays.iter().map(|d| d.id.clone()).collect();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), state.displays.len(), "display ids repeat");
            for display in &state.displays {
                assert!(display.width > 0 && display.height > 0);
                assert!(display.scale >= 1.0, "{display:?}");
            }
            assert!(state.locked.is_some());
        }
        Err(error) => {
            // A runner whose tests run as a service (session 0) has no desktop, and says so.
            assert_eq!(error.code, ErrorCode::Unavailable);
            assert!(error.message.contains("session 0"), "{error}");
        }
    }
}

#[test]
fn a_clipboard_text_over_the_limit_is_refused_before_the_clipboard_is_opened() {
    let desktop = WindowsDesktop::new(WindowsSettings {
        clipboard_max_bytes: 8,
        ..WindowsSettings::default()
    });
    let error = desktop.set_clipboard_text("123456789").unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest);
    let error = desktop.set_clipboard_text("a\0b").unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest);
}

/// While the secure desktop has the screen (the lock screen, a sign-in or an elevation prompt) nothing of the screen is
/// captured and nothing is sent to it, and the refusal says why. This test captures and sends nothing when the screen is
/// not locked, so it runs wherever the tests do; it proves its claim only on a locked screen.
#[test]
fn while_the_secure_desktop_has_the_screen_capture_and_input_are_refused_with_the_reason() {
    let desktop = desktop(WindowsCapture::Auto);
    let Ok(state) = desktop.state() else { return };
    if state.locked != Some(true) {
        println!("NOT LOCKED: the secure-desktop refusals were not exercised on this screen");
        return;
    }
    let display = state.display(None).expect("a display").id.clone();
    for method in [
        WindowsCapture::Auto,
        WindowsCapture::Dxgi,
        WindowsCapture::Gdi,
    ] {
        let error = self::desktop(method)
            .capture(&display, Some(Rect::new(0, 0, 4, 4)))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Unavailable, "{method:?}");
        assert!(error.message.contains("secure desktop"), "{error}");
    }
    let error = desktop
        .input(&[InputEvent::Move {
            display: None,
            x: 1,
            y: 1,
        }])
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("secure desktop"), "{error}");
    println!(
        "LOCKED: capture (three methods) and input were refused with the secure desktop's reason"
    );
}

#[test]
fn a_batch_with_a_point_off_its_display_is_refused_whole() {
    let desktop = desktop(WindowsCapture::Auto);
    let Ok(state) = desktop.state() else { return };
    if state.locked != Some(false) || !state.active {
        return;
    }
    let primary = state.display(None).expect("a display").clone();
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
    assert_eq!(error.code, ErrorCode::BadRequest);
    assert!(error.message.contains("outside the display"), "{error}");
}

/// The colour of the probe's cell `cx`, `cy`, as `gui_probe.ps1` paints it.
fn cell(cx: u32, cy: u32) -> [u8; 3] {
    [
        u8::try_from(30 + 50 * cx).unwrap(),
        u8::try_from(30 + 50 * cy).unwrap(),
        u8::try_from(60 + 30 * ((cx + cy) % 4)).unwrap(),
    ]
}

fn pattern() -> Vec<u8> {
    let mut rgb = Vec::new();
    for y in 0..128_u32 {
        for x in 0..128_u32 {
            rgb.extend_from_slice(&cell(x / 32, y / 32));
        }
    }
    rgb
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
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/gui_probe.ps1");
        let shell = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let mut child = Command::new(shell)
            .args(["-NoProfile", "-STA", "-File"])
            .arg(script)
            .args([
                "-X",
                &x.to_string(),
                "-Y",
                &y.to_string(),
                "-Seconds",
                "120",
            ])
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

    /// Lets the probe's events arrive, then says whether it has the focus.
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

/// A window of the probe's size at `x`, `y` on the primary display, left clear of its edges.
fn probe_place(desktop: &WindowsDesktop) -> (DisplayId, i32, i32, i32, i32) {
    let state = desktop.state().expect("a desktop");
    let primary = state.display(None).expect("a display");
    (
        primary.id.clone(),
        primary.x,
        primary.y,
        primary.x + 120,
        primary.y + 120,
    )
}

/// Puts the pointer back where it was when the test started, even when it fails.
struct PutPointerBack<'a> {
    desktop: &'a WindowsDesktop,
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
    desktop: &'a WindowsDesktop,
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

/// The text the probe's box holds after the last `text` line it said.
fn last_text(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix("text"))
        .map(|escaped| {
            let escaped = escaped.trim_start();
            let mut bytes = Vec::new();
            let mut chars = escaped.bytes();
            while let Some(byte) = chars.next() {
                if byte == b'%' {
                    let high = chars.next().unwrap();
                    let low = chars.next().unwrap();
                    let digits = [high, low];
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
    if !live("the_probe_window_is_captured_and_driven") {
        return;
    }
    let desktop = desktop(WindowsCapture::Auto);
    match desktop.state() {
        Ok(state) if state.active && state.locked == Some(false) => {}
        other => {
            println!(
                "SKIPPED: the_probe_window_is_captured_and_driven: no unlocked interactive desktop ({other:?})"
            );
            return;
        }
    }
    let (display, left, top, x, y) = probe_place(&desktop);
    let back = PutPointerBack {
        desktop: &desktop,
        at: desktop.cursor(),
    };
    let mut probe = Probe::start(x, y);
    let ready = probe.until("ready", |line| line.starts_with("ready "));
    let numbers: Vec<i32> = ready
        .last()
        .unwrap()
        .split_whitespace()
        .skip(1)
        .map(|n| n.parse().unwrap())
        .collect();
    assert_eq!(
        numbers,
        [x, y, 128, 128],
        "the probe is not where it was put (DPI?)"
    );
    let region = Rect::new((x - left) as u32, (y - top) as u32, 128, 128);

    // Capture: the probe's own region, by each method, equals the pattern it painted.
    let expected = pattern();
    for capture in [
        WindowsCapture::Dxgi,
        WindowsCapture::Gdi,
        WindowsCapture::Auto,
    ] {
        let desktop = self::desktop(capture);
        let deadline = Instant::now() + Duration::from_secs(10);
        let frame = loop {
            let frame = desktop
                .capture(&display, Some(region))
                .unwrap_or_else(|error| panic!("{capture:?} did not capture: {error}"));
            if frame.rgb() == expected.as_slice() || Instant::now() > deadline {
                break frame;
            }
            thread::sleep(Duration::from_millis(100));
        };
        assert_eq!((frame.width(), frame.height()), (128, 128));
        let differing = frame
            .rgb()
            .as_chunks::<3>()
            .0
            .iter()
            .zip(expected.as_chunks::<3>().0)
            .filter(|(got, want)| got != want)
            .count();
        assert_eq!(
            differing, 0,
            "{capture:?}: {differing} of 16384 pixels differ from the pattern"
        );
        println!("capture {capture:?}: the probe's 128x128 equals its pattern");
    }

    // Clicks land on the exact pixel, at both corners and in the middle.
    for (dx, dy) in [(0, 0), (127, 127), (64, 64), (10, 100)] {
        let _ = probe.drain();
        let applied = desktop
            .input(&[click(&display, x - left + dx, y - top + dy, Button::Left)])
            .expect("a click");
        assert_eq!(applied, 1);
        let lines = probe.until("the click", |line| line.starts_with("mouse "));
        let said = lines.last().unwrap();
        assert_eq!(
            said,
            &format!("mouse left {} {}", x + dx, y + dy),
            "a click at ({dx}, {dy}) of the probe landed elsewhere"
        );
    }
    println!("click: four points of the probe, each exactly where it was aimed");

    // Right and middle report their own button; a double click is two.
    for (button, name) in [(Button::Right, "right"), (Button::Middle, "middle")] {
        desktop
            .input(&[click(&display, x - left + 20, y - top + 20, button)])
            .unwrap();
        let lines = probe.until(name, |line| line.starts_with("mouse "));
        assert_eq!(
            lines.last().unwrap(),
            &format!("mouse {name} {} {}", x + 20, y + 20)
        );
    }
    let moved = desktop.cursor().expect("a pointer");
    assert_eq!(moved, (display.clone(), x - left + 20, y - top + 20));

    // Keyboard: only while the probe says it has the focus.
    let _ = probe.drain();
    if !probe.has_focus() {
        desktop
            .input(&[click(&display, x - left + 64, y - top + 64, Button::Left)])
            .unwrap();
        thread::sleep(Duration::from_millis(300));
    }
    assert!(
        probe.has_focus(),
        "the probe does not have the focus, so nothing is typed; it said {:?}",
        probe.seen
    );
    let _ = probe.drain();
    // One character at a time, the focus looked at before each: a window that took the focus meanwhile is not typed into.
    let typed = type_while_focused(
        "abc é",
        |character| {
            desktop
                .input(&[InputEvent::Text {
                    text: character.to_owned(),
                }])
                .unwrap();
            thread::sleep(Duration::from_millis(80));
        },
        || probe.has_focus(),
    );
    let lines = probe.drain();
    assert_eq!(
        typed,
        Ok(5),
        "the probe lost the focus while it was typed to; it said {:?}",
        probe.seen
    );
    assert!(probe.focused, "the probe lost the focus: {lines:?}");
    assert_eq!(last_text(&lines).as_deref(), Some("abc é"), "{lines:?}");
    let chars: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.strip_prefix("char "))
        .collect();
    assert_eq!(
        chars,
        ["U+0061", "U+0062", "U+0063", "U+0020", "U+00E9"],
        "{lines:?}"
    );
    println!("text: ASCII and a non-ASCII character reached the probe's box in order");

    // A combo: control+a selects all, and `end` only moves the caret, which is harmless in any window that has the focus
    // instead (a destructive key here would delete the owner's text there).
    assert!(probe.has_focus(), "the probe lost the focus");
    desktop
        .input(&[
            key("control", KeyAction::Press),
            key("a", KeyAction::Click),
            key("control", KeyAction::Release),
            key("end", KeyAction::Click),
        ])
        .unwrap();
    thread::sleep(Duration::from_millis(400));
    let lines = probe.drain();
    assert!(
        lines.iter().any(|line| line.starts_with("down A ctrl=1")),
        "control+a did not arrive as a combo: {lines:?}"
    );
    assert!(
        lines.iter().all(|line| !line.starts_with("text"))
            || last_text(&lines).as_deref() == Some("abc é"),
        "the box changed: {lines:?}"
    );
    println!("combo: control+a then end arrived as keys and left the probe's box as typed");

    // The wheel: positive dy is down (negative at Windows), dx to the right.
    desktop
        .input(&[
            InputEvent::Move {
                display: Some(display.clone()),
                x: x - left + 64,
                y: y - top + 64,
            },
            InputEvent::Scroll {
                display: None,
                x: None,
                y: None,
                dx: 1,
                dy: 2,
            },
        ])
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    let lines = probe.drain();
    assert!(lines.iter().any(|line| line == "wheel V -240"), "{lines:?}");
    assert!(lines.iter().any(|line| line == "wheel H 120"), "{lines:?}");
    println!("scroll: two notches down and one right arrived as -240 and 120");

    // The clipboard's text, put back after.
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
                "SKIPPED: clipboard round trip: the clipboard holds no text ({message}), and a test must not replace it"
            );
        }
    }
    drop(back);
}

/// How fast the screen is captured, as sanity numbers (a local run is not evidence of performance). The pixels are
/// dropped at once and never printed or written.
#[test]
#[ignore = "measures captures of the primary display; needs an unlocked desktop and BOHIO_TEST_GUI_LIVE=1"]
fn how_fast_the_primary_display_is_captured() {
    if !live("how_fast_the_primary_display_is_captured") {
        return;
    }
    let probe = desktop(WindowsCapture::Auto);
    let state = match probe.state() {
        Ok(state) if state.active && state.locked == Some(false) => state,
        other => {
            println!(
                "SKIPPED: how_fast_the_primary_display_is_captured: no unlocked interactive desktop ({other:?})"
            );
            return;
        }
    };
    let primary = state.display(None).expect("a display").clone();
    for capture in [WindowsCapture::Dxgi, WindowsCapture::Gdi] {
        let desktop = self::desktop(capture);
        for (name, region) in [
            ("whole display", None),
            ("128x128", Some(Rect::new(0, 0, 128, 128))),
        ] {
            // The first capture opens the duplication; it is not a rate.
            let first = Instant::now();
            if let Err(error) = desktop.capture(&primary.id, region) {
                println!("{capture:?} {name}: {error}");
                continue;
            }
            let first = first.elapsed();
            let mut each = Vec::new();
            let start = Instant::now();
            while each.len() < 60 && start.elapsed() < Duration::from_secs(20) {
                let one = Instant::now();
                drop(desktop.capture(&primary.id, region).expect("a capture"));
                each.push(one.elapsed());
            }
            let total = start.elapsed();
            each.sort();
            println!(
                "{capture:?} {name} ({}x{}): first {first:?}, {} captures in {total:?} = {:.1} a second, median {:?}, slowest {:?}",
                region.map_or(primary.width, |r| r.width),
                region.map_or(primary.height, |r| r.height),
                each.len(),
                each.len() as f64 / total.as_secs_f64(),
                each[each.len() / 2],
                each[each.len() - 1],
            );
        }
    }
}

/// Types `text` one character at a time through `send`, asking `focused` before each: the first time it says no, nothing more
/// is sent. The count typed, or how many went before the focus was lost.
fn type_while_focused(
    text: &str,
    mut send: impl FnMut(&str),
    mut focused: impl FnMut() -> bool,
) -> Result<usize, usize> {
    let mut typed = 0;
    for character in text.chars() {
        if !focused() {
            return Err(typed);
        }
        send(&character.to_string());
        typed += 1;
    }
    Ok(typed)
}

#[test]
fn typing_stops_at_the_first_character_after_the_focus_is_lost() {
    let mut sent = Vec::new();
    let mut asked = 0;
    let result = type_while_focused(
        "abc é",
        |character| sent.push(character.to_owned()),
        || {
            asked += 1;
            asked < 2
        },
    );
    assert_eq!(result, Err(1));
    assert_eq!(
        sent,
        ["a"],
        "a character went to a window that had the focus"
    );
    // The control: a probe that keeps the focus gets every character, one call each.
    let mut sent = Vec::new();
    assert_eq!(
        type_while_focused("abc é", |c| sent.push(c.to_owned()), || true),
        Ok(5)
    );
    assert_eq!(sent, ["a", "b", "c", " ", "é"]);
}
