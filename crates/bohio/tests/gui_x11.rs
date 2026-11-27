//! The X11 desktop backend against an X display.
//!
//! Nothing here touches a display unless `BOHIO_TEST_GUI_LIVE=1`; without it the live tests print `SKIPPED` with the
//! reason. With it a test serves its own `Xvfb` when one is on `PATH` (never the ambient display), else uses the display
//! `BOHIO_TEST_X11_DISPLAY` names (an existing server, WSLg's for one): then only a window the test made itself
//! (`examples/gui_probe_x11.rs`) is captured and driven, its pattern is the only thing compared, the pointer is put back,
//! keys are typed only while the probe reports the keyboard focus and a click is made only on a point nothing covers; the
//! monitors are not redefined, and the clipboard test uses a selection of its own, which no real clipboard follows. No
//! image is written anywhere.
//!
//! The probe is built by `cargo build -p bohio --example gui_probe_x11`.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

#[path = "../examples/support/x11_pattern.rs"]
mod pattern;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{GuiConfig, LinkConfig, NodeConfig, Span};
use bohio::gui::{Desktop, X11Desktop, X11Settings};
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_client::control::Control;
use bohio_client::gui::{Gui, Snapshot};
use bohio_client::link::{Settings, stream};
use bohio_proto::ErrorCode;
use bohio_proto::gui::{
    Button, ButtonAction, DisplayId, GuiRequest, ImageFormat, InputEvent, KeyAction, Rect, Want,
};
use cabuya::auth::{Identity, Method};
use coqui_peer::PeerId;
use coqui_transport::{Channel, StreamChannel, memory};
use x11rb::connection::Connection;
use x11rb::protocol::randr::{ConnectionExt as _, MonitorInfo};
use x11rb::protocol::xproto::{ConnectionExt as _, MapState};
use x11rb::rust_connection::RustConnection;

const WAIT: Duration = Duration::from_secs(30);
/// How long a test waits for what the probe should print before it calls it missing.
const GIVE_UP: Duration = Duration::from_secs(10);

/// One live test at a time: they share a display (and its one pointer).
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn live(test: &str) -> bool {
    if std::env::var("BOHIO_TEST_GUI_LIVE").as_deref() == Ok("1") {
        return true;
    }
    println!(
        "SKIPPED: {test}: BOHIO_TEST_GUI_LIVE=1 is not set, so no display is captured or driven"
    );
    false
}

/// A display for a test: a server of its own, or one that exists.
struct Server {
    display: String,
    /// Whether this test started it, so that it may define monitors and own the whole screen.
    private: bool,
    child: Option<Child>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn xvfb() -> Option<Server> {
    let mut child = Command::new("Xvfb")
        .args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            "1280x800x24",
            "-nolisten",
            "tcp",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = send.send(line);
    });
    match receive.recv_timeout(WAIT) {
        Ok(line) if !line.trim().is_empty() => Some(Server {
            display: format!(":{}", line.trim()),
            private: true,
            child: Some(child),
        }),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// The display a live test runs on, or `None` (after saying why) when there is none.
fn server(test: &str) -> Option<Server> {
    if !live(test) {
        return None;
    }
    if let Some(server) = xvfb() {
        return Some(server);
    }
    match std::env::var("BOHIO_TEST_X11_DISPLAY") {
        Ok(display) if !display.is_empty() => Some(Server {
            display,
            private: false,
            child: None,
        }),
        _ => {
            println!(
                "SKIPPED: {test}: no Xvfb on PATH and BOHIO_TEST_X11_DISPLAY names no display"
            );
            None
        }
    }
}

fn desktop(server: &Server) -> X11Desktop {
    X11Desktop::new({
        let mut s = X11Settings::default();
        s.display = Some(server.display.clone());
        s
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The probe window.

fn probe_path() -> PathBuf {
    let exe = std::env::current_exe().expect("the test's own path");
    exe.ancestors()
        .skip(1)
        .map(|directory| directory.join("examples").join("gui_probe_x11"))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!(
                "the probe is not built: run `cargo build -p bohio --example gui_probe_x11` first (looked above {})",
                exe.display()
            )
        })
}

struct Probe {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    reader: Option<JoinHandle<()>>,
    /// Where its pixels are in the root, and their size.
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    window: u32,
    seen: Vec<String>,
}

impl Probe {
    fn start(display: &str, at: Option<(i32, i32)>) -> Self {
        let mut command = Command::new(probe_path());
        command.arg(display);
        if let Some((x, y)) = at {
            command.arg(x.to_string()).arg(y.to_string());
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the probe starts");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("its output");
        let (send, lines) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let mut probe = Self {
            child,
            stdin,
            lines,
            reader: Some(reader),
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            window: 0,
            seen: Vec::new(),
        };
        let ready = probe
            .wait_for(|line| line.starts_with("ready "), WAIT)
            .expect("the probe reports its window");
        probe.take_place(&ready);
        probe
    }

    fn take_place(&mut self, ready: &str) {
        let numbers: Vec<i64> = ready
            .split_whitespace()
            .skip(1)
            .filter_map(|word| word.parse().ok())
            .collect();
        assert_eq!(numbers.len(), 5, "{ready}");
        self.x = i32::try_from(numbers[0]).unwrap();
        self.y = i32::try_from(numbers[1]).unwrap();
        self.width = u32::try_from(numbers[2]).unwrap();
        self.height = u32::try_from(numbers[3]).unwrap();
        self.window = u32::try_from(numbers[4]).unwrap();
    }

    /// Waits until the window is placed on one of the displays: a compositor puts a new window far off the screen until it
    /// has placed it, and says so with another `ready` line.
    fn placed_on(&mut self, state: &bohio::gui::DesktopState) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while display_of(state, self.x, self.y).is_none() {
            let Some(left) = until.checked_duration_since(Instant::now()) else {
                return false;
            };
            let Some(ready) = self.wait_for(|line| line.starts_with("ready "), left) else {
                return false;
            };
            self.take_place(&ready);
        }
        true
    }

    /// The first line, from here on, that `matches`; lines before it are kept in `seen`.
    fn wait_for(&mut self, matches: impl Fn(&str) -> bool, within: Duration) -> Option<String> {
        let until = Instant::now() + within;
        loop {
            let left = until.checked_duration_since(Instant::now())?;
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if matches(&line) {
                        return Some(line);
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// The `key` and `button` lines the probe prints from now until nothing more comes for `quiet`, once `at_least` have.
    fn events_for(&mut self, quiet: Duration, at_least: usize) -> Vec<String> {
        let mut found = Vec::new();
        let until = Instant::now() + GIVE_UP;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            let wait = if found.len() >= at_least { quiet } else { left };
            let Ok(line) = self.lines.recv_timeout(wait) else {
                break;
            };
            self.seen.push(line.clone());
            if line.starts_with("key ") || line.starts_with("button ") {
                found.push(line);
            }
        }
        found
    }

    fn focused(&mut self, within: Duration) -> bool {
        // A `focused` line may have been printed before this call, and be in `seen` already.
        let mut last = self
            .seen
            .iter()
            .rev()
            .find(|line| *line == "focused" || *line == "unfocused")
            .cloned();
        if last.as_deref() != Some("focused") {
            last = self.wait_for(|line| line == "focused", within);
        }
        last.as_deref() == Some("focused")
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Puts the pointer back where it was.
struct Pointer<'a> {
    desktop: &'a X11Desktop,
    saved: Option<(DisplayId, i32, i32)>,
}

impl<'a> Pointer<'a> {
    fn save(desktop: &'a X11Desktop) -> Self {
        Self {
            desktop,
            saved: desktop.cursor(),
        }
    }
}

impl Drop for Pointer<'_> {
    fn drop(&mut self) {
        if let Some((display, x, y)) = self.saved.take() {
            let _ = self.desktop.input(&[InputEvent::Move {
                display: Some(display),
                x,
                y,
            }]);
        }
    }
}

/// Whether nothing mapped lies over `point` above the probe's window in the root's stacking order.
fn uncovered(display: &str, window: u32, point: (i32, i32)) -> bool {
    let Ok((connection, number)) = RustConnection::connect(Some(display)) else {
        return false;
    };
    let root = connection.setup().roots[number].root;
    let Some(children) = connection
        .query_tree(root)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|tree| tree.children)
    else {
        return false;
    };
    // Bottom to top: the windows after the probe's (or after its top-level ancestor) are above it.
    let ours = {
        let mut current = window;
        loop {
            let parent = connection
                .query_tree(current)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|tree| tree.parent);
            match parent {
                Some(parent) if parent == root => break current,
                Some(parent) if parent != 0 => current = parent,
                _ => return false,
            }
        }
    };
    let Some(position) = children.iter().position(|child| *child == ours) else {
        return false;
    };
    children[position + 1..].iter().all(|child| {
        let mapped = connection
            .get_window_attributes(*child)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|attributes| attributes.map_state == MapState::VIEWABLE);
        if !mapped {
            return true;
        }
        let Some(geometry) = connection
            .get_geometry(*child)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            return true;
        };
        let (x, y) = (i32::from(geometry.x), i32::from(geometry.y));
        let over = point.0 >= x
            && point.1 >= y
            && point.0 < x + i32::from(geometry.width)
            && point.1 < y + i32::from(geometry.height);
        !over
    })
}

/// The display that holds the root point, and the point on it.
fn display_of(
    state: &bohio::gui::DesktopState,
    x: i32,
    y: i32,
) -> Option<(bohio_proto::gui::Display, i32, i32)> {
    state.displays.iter().find_map(|display| {
        let inside = x >= display.x
            && y >= display.y
            && i64::from(x) < i64::from(display.x) + i64::from(display.width)
            && i64::from(y) < i64::from(display.y) + i64::from(display.height);
        inside.then(|| (display.clone(), x - display.x, y - display.y))
    })
}

/// How many pixels of `got` differ from the pattern, and the first one that does.
fn differences(got: &[u8]) -> (usize, Option<(usize, usize)>) {
    let want = pattern::rgb();
    let width = usize::from(pattern::WIDTH);
    let mut count = 0;
    let mut first = None;
    for (index, (a, b)) in got.chunks(3).zip(want.chunks(3)).enumerate() {
        if a != b {
            count += 1;
            first.get_or_insert((index % width, index / width));
        }
    }
    (count, first)
}

// ---------------------------------------------------------------------------------------------------------------
// What needs no display.

#[test]
fn a_node_with_no_display_has_no_desktop_and_says_so() {
    let desktop = X11Desktop::new(X11Settings::default());
    let error = desktop.state().unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("no X display"), "{error}");
    let capabilities = desktop.capabilities();
    assert_eq!(capabilities.backend, "x11");
}

#[test]
fn a_display_that_is_not_there_is_unavailable_and_names_it() {
    let desktop = X11Desktop::new({
        let mut s = X11Settings::default();
        s.display = Some(":97".to_owned());
        s
    });
    for error in [
        desktop.state().unwrap_err(),
        desktop
            .capture(&DisplayId("display1".to_owned()), None)
            .unwrap_err(),
        desktop.input(&[]).unwrap_err(),
        desktop.clipboard_text().unwrap_err(),
    ] {
        assert_eq!(error.code, ErrorCode::Unavailable);
        assert!(error.message.contains(":97"), "{error}");
    }
    assert!(desktop.cursor().is_none());
}

#[test]
fn a_wayland_session_is_refused_with_the_way_to_serve_its_x11_windows() {
    let refused = X11Desktop::new({
        let mut s = X11Settings::default();
        s.display = Some(":97".to_owned());
        s.wayland = true;
        s
    });
    let error = refused.state().unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("Wayland"), "{error}");
    assert!(error.message.contains("x11_in_wayland"), "{error}");
    // Asked for, it goes on to the display (which is not there).
    let allowed = X11Desktop::new({
        let mut s = X11Settings::default();
        s.display = Some(":97".to_owned());
        s.wayland = true;
        s.in_wayland = true;
        s
    });
    let error = allowed.state().unwrap_err();
    assert!(error.message.contains("cannot connect"), "{error}");
}

// ---------------------------------------------------------------------------------------------------------------
// Against a display.

#[test]
fn the_state_names_the_displays() {
    let _turn = turn();
    let Some(server) = server("the_state_names_the_displays") else {
        return;
    };
    let desktop = desktop(&server);
    let capabilities = desktop.capabilities();
    assert_eq!(capabilities.backend, "x11");
    assert!(capabilities.input && capabilities.clipboard);
    let state = desktop.state().expect("a state");
    assert!(state.active);
    assert!(!state.displays.is_empty());
    assert_eq!(state.displays.iter().filter(|d| d.primary).count(), 1);
    let mut ids: Vec<_> = state.displays.iter().map(|d| d.id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), state.displays.len(), "display ids repeat");
    for display in &state.displays {
        assert!(display.width > 0 && display.height > 0);
    }
    println!(
        "DISPLAYS ({}): {}",
        if server.private {
            "private server"
        } else {
            "ambient server"
        },
        state
            .displays
            .iter()
            .map(|d| format!("{} {}x{}+{}+{}", d.id.as_str(), d.width, d.height, d.x, d.y))
            .collect::<Vec<_>>()
            .join(", ")
    );
    // The pointer is on one of them.
    if let Some((id, x, y)) = desktop.cursor() {
        let display = state
            .displays
            .iter()
            .find(|d| d.id == id)
            .expect("a listed display");
        assert!(x >= 0 && y >= 0 && x < display.width as i32 && y < display.height as i32);
    }
}

#[test]
fn lock_detection_is_logind_or_nothing() {
    let _turn = turn();
    let Some(server) = server("lock_detection_is_logind_or_nothing") else {
        return;
    };
    // A system bus that is not there: no detection, an unknown lock state, and the screen is still served.
    let without = X11Desktop::new({
        let mut s = X11Settings::default();
        s.display = Some(server.display.clone());
        s.system_bus = Some("unix:path=/nonexistent/bohio-test-bus".to_owned());
        s
    });
    assert!(!without.capabilities().lock_detection);
    let state = without.state().expect("a state without logind");
    assert_eq!(state.locked, None);
    assert!(state.active && !state.displays.is_empty());
    // The standard bus: whichever it says, `info` and the state agree. A session id may be named for a shell that belongs to no
    // session (`BOHIO_TEST_X11_SESSION`).
    let with = X11Desktop::new({
        let mut s = X11Settings::default();
        s.display = Some(server.display.clone());
        s.session = std::env::var("BOHIO_TEST_X11_SESSION").ok();
        s
    });
    let detected = with.capabilities().lock_detection;
    let state = with.state().expect("a state");
    assert_eq!(state.locked.is_some(), detected);
    println!(
        "LOGIND: lock_detection={detected} locked={:?}",
        state.locked
    );
}

#[test]
fn the_probe_window_is_captured_and_driven() {
    let _turn = turn();
    let Some(server) = server("the_probe_window_is_captured_and_driven") else {
        return;
    };
    let desktop = desktop(&server);
    let state = desktop.state().expect("a state");
    let mut probe = Probe::start(&server.display, server.private.then_some((40, 30)));
    probe.placed_on(&state);
    let (display, left, top) = display_of(&state, probe.x, probe.y).unwrap_or_else(|| {
        panic!(
            "the probe's window at {},{} is on no display of {:?}",
            probe.x, probe.y, state.displays
        )
    });
    assert_eq!((probe.width, probe.height), (128, 96));
    let region = Rect::new(
        u32::try_from(left).unwrap(),
        u32::try_from(top).unwrap(),
        probe.width,
        probe.height,
    );

    // Capture: the probe's pixels, and only they.
    let frame = desktop
        .capture(&display.id, Some(region))
        .expect("the probe's region is captured");
    assert_eq!((frame.width(), frame.height()), (128, 96));
    let (different, first) = differences(frame.rgb());
    println!(
        "CAPTURE ({}): {different} of {} pixels differ from the pattern, first at {first:?}",
        if server.private {
            "private server"
        } else {
            "ambient server"
        },
        128 * 96
    );
    assert_eq!(
        different, 0,
        "the capture of the probe's window is not its pattern"
    );

    // A sub-region is the same pixels at its own offset.
    let part = desktop
        .capture(
            &display.id,
            Some(Rect::new(region.x + 16, region.y + 8, 32, 16)),
        )
        .expect("a part");
    let want = pattern::rgb();
    for row in 0..16_usize {
        let from = ((8 + row) * 128 + 16) * 3;
        assert_eq!(
            &part.rgb()[row * 32 * 3..(row + 1) * 32 * 3],
            &want[from..from + 32 * 3],
            "row {row} of the part"
        );
    }

    // A batch with a point off the display is refused whole.
    let error = desktop
        .input(&[
            InputEvent::Move {
                display: Some(display.id.clone()),
                x: 1,
                y: 1,
            },
            InputEvent::Move {
                display: Some(display.id.clone()),
                x: i32::try_from(display.width).unwrap(),
                y: 0,
            },
        ])
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest);

    let centre = (probe.x + 64, probe.y + 48);
    if !uncovered(&server.display, probe.window, centre) {
        println!(
            "SKIPPED: input: another window covers the probe's centre, so nothing is sent there"
        );
        return;
    }
    // A compositor that has just placed the window may not yet send it the pointer: wait for the keyboard focus it
    // takes at the same time, and a moment more.
    let _ = probe.focused(Duration::from_secs(3));
    thread::sleep(Duration::from_millis(200));
    let before = desktop.cursor();
    let pointer = Pointer::save(&desktop);
    let at = |dx: i32, dy: i32| (left + dx, top + dy);
    let (cx, cy) = at(64, 48);

    // The pointer goes to the probe's centre first. A server that is XWayland of a compositor may leave it where it was
    // (measured on WSLg: in some runs the pointer did not move at all), and then nothing below can be proved: a server this
    // test started must move it.
    let mut moved = false;
    for _ in 0..10 {
        desktop
            .input(&[InputEvent::Move {
                display: Some(display.id.clone()),
                x: cx,
                y: cy,
            }])
            .expect("a move");
        thread::sleep(Duration::from_millis(200));
        if desktop.cursor() == Some((display.id.clone(), cx, cy)) {
            moved = true;
            break;
        }
    }
    if !moved {
        assert!(
            !server.private,
            "the pointer did not move on a server this test started"
        );
        println!(
            "SKIPPED: input: the display did not move its pointer to the probe (it is at {:?}), so no click or key is sent",
            desktop.cursor()
        );
        return;
    }

    // Clicks land on the root point: a left click, a right click, the wheel.
    let applied = desktop
        .input(&[
            InputEvent::Button {
                button: Button::Left,
                action: ButtonAction::Click,
                count: None,
                display: Some(display.id.clone()),
                x: Some(cx),
                y: Some(cy),
            },
            InputEvent::Button {
                button: Button::Right,
                action: ButtonAction::Click,
                count: None,
                display: None,
                x: None,
                y: None,
            },
            InputEvent::Scroll {
                display: None,
                x: None,
                y: None,
                dx: 1,
                dy: 2,
            },
        ])
        .expect("clicks are sent");
    assert_eq!(applied, 3);
    let events = probe.events_for(Duration::from_millis(500), 6);
    let (rx, ry) = (centre.0, centre.1);
    assert_eq!(
        events,
        [
            format!("button 1 {rx} {ry}"),
            format!("button 3 {rx} {ry}"),
            format!("button 5 {rx} {ry}"),
            format!("button 5 {rx} {ry}"),
            format!("button 7 {rx} {ry}"),
        ],
        "what the probe received (the pointer is at {:?}; the probe saw {:?})",
        desktop.cursor(),
        probe.seen
    );
    // The pointer is reported where it was put.
    let (id, px, py) = desktop.cursor().expect("a pointer");
    assert_eq!((id, px, py), (display.id.clone(), cx, cy));

    // Typing: only while the probe has the keyboard focus.
    if !probe.focused(Duration::from_secs(3)) {
        println!(
            "SKIPPED: typing: the probe never reported the keyboard focus, so no key is sent (seen: {:?})",
            probe.seen
        );
        return;
    }
    desktop
        .input(&[InputEvent::Text {
            text: "aBé".to_owned(),
        }])
        .expect("text is typed");
    let events = probe.events_for(Duration::from_millis(500), 3);
    assert_eq!(
        events,
        ["key U+0061", "key keysym:FFE1", "key U+0042", "key U+00E9"],
        "ASCII, a shifted capital (with the shift key before it) and a character the keyboard has no key for"
    );
    desktop
        .input(&[
            InputEvent::Key {
                key: "control".to_owned(),
                action: KeyAction::Press,
            },
            InputEvent::Key {
                key: "a".to_owned(),
                action: KeyAction::Click,
            },
            InputEvent::Key {
                key: "control".to_owned(),
                action: KeyAction::Release,
            },
        ])
        .expect("a combination is sent");
    let events = probe.events_for(Duration::from_millis(500), 2);
    assert_eq!(events, ["key keysym:FFE3", "key U+0061"]);
    // The keyboard mapping is as it was: the spare key is free again.
    let again = desktop
        .input(&[InputEvent::Text {
            text: "é".to_owned(),
        }])
        .expect("typed again");
    assert_eq!(again, 1);
    assert_eq!(
        probe.events_for(Duration::from_millis(500), 1),
        ["key U+00E9"]
    );
    drop(pointer);
    assert_eq!(desktop.cursor(), before, "the pointer is back where it was");
}

#[test]
fn a_clipboard_text_round_trips_through_a_selection_of_its_own() {
    let _turn = turn();
    let Some(server) = server("a_clipboard_text_round_trips_through_a_selection_of_its_own") else {
        return;
    };
    let name = format!("BOHIO_TEST_{}", std::process::id());
    let made = |selection: &str, max: usize| {
        X11Desktop::new({
            let mut s = X11Settings::default();
            s.display = Some(server.display.clone());
            s.selection = selection.to_owned();
            s.clipboard_max_bytes = max;
            s
        })
    };
    let desktop = made(&name, 1024);
    // Nobody owns it yet.
    assert_eq!(desktop.clipboard_text().expect("an empty read"), "");
    let text = "line one\nline two é 😀";
    desktop.set_clipboard_text(text).expect("set");
    assert_eq!(desktop.clipboard_text().expect("read back"), text);
    // Another desktop (another connection) reads what this one owns.
    assert_eq!(made(&name, 1024).clipboard_text().expect("read"), text);
    // A second text replaces the first.
    desktop.set_clipboard_text("second").expect("set again");
    assert_eq!(desktop.clipboard_text().expect("read"), "second");
    // Over the limit is refused on the way in and on the way out.
    let error = desktop.set_clipboard_text(&"x".repeat(2000)).unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest);
    let small = made(&name, 3);
    let error = small.clipboard_text().unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest);
}

/// The times the clipboard's owner thread has gone to sleep and woken, from the kernel's own count for the thread.
fn owner_wakeups() -> Option<u64> {
    for entry in std::fs::read_dir("/proc/self/task").ok()?.flatten() {
        let path = entry.path();
        // The kernel keeps the first 15 bytes of a thread's name.
        if std::fs::read_to_string(path.join("comm")).ok()?.trim() != "bohio-x11-clipb" {
            continue;
        }
        let status = std::fs::read_to_string(path.join("status")).ok()?;
        let line = status
            .lines()
            .find(|line| line.starts_with("voluntary_ctxt_switches"))?;
        return line.split_whitespace().nth(1)?.parse().ok();
    }
    None
}

#[test]
fn an_owner_of_the_clipboard_that_nobody_asks_does_not_wake() {
    let _turn = turn();
    let Some(server) = server("an_owner_of_the_clipboard_that_nobody_asks_does_not_wake") else {
        return;
    };
    let desktop = X11Desktop::new({
        let mut s = X11Settings::default();
        s.display = Some(server.display.clone());
        s.selection = format!("BOHIO_TEST_IDLE_{}", std::process::id());
        s
    });
    desktop.set_clipboard_text("held").expect("set");
    // Let the thread settle into waiting, then count a second of nobody asking.
    thread::sleep(Duration::from_millis(300));
    let before = owner_wakeups().expect("the owner thread");
    thread::sleep(Duration::from_secs(1));
    let after = owner_wakeups().expect("the owner thread");
    // Polling every 5 ms woke it about 200 times in that second.
    assert!(
        after - before < 10,
        "the owner woke {} times",
        after - before
    );
    // It still answers, and is ended by a new owner without waiting out a poll.
    assert_eq!(desktop.clipboard_text().expect("read"), "held");
    let started = Instant::now();
    drop(desktop);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_click_on_the_second_monitor_lands_at_its_root_point() {
    let _turn = turn();
    let Some(server) = server("a_click_on_the_second_monitor_lands_at_its_root_point") else {
        return;
    };
    if !server.private {
        println!(
            "SKIPPED: a_click_on_the_second_monitor_lands_at_its_root_point: monitors are defined only on a server this test started"
        );
        return;
    }
    // Two monitors side by side, defined on the root, as a second display would be.
    let (connection, number) = RustConnection::connect(Some(&server.display)).expect("a display");
    let root = connection.setup().roots[number].root;
    let atom = |name: &str| {
        connection
            .intern_atom(false, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    for (name, x) in [("bohio-left", 0_i16), ("bohio-right", 640)] {
        connection
            .randr_set_monitor(
                root,
                MonitorInfo {
                    name: atom(name),
                    primary: x == 0,
                    automatic: false,
                    x,
                    y: 0,
                    width: 640,
                    height: 800,
                    width_in_millimeters: 169,
                    height_in_millimeters: 211,
                    outputs: Vec::new(),
                },
            )
            .unwrap()
            .check()
            .unwrap();
    }
    let desktop = desktop(&server);
    let state = desktop.state().expect("a state");
    let right = state
        .displays
        .iter()
        .find(|d| d.id.as_str() == "bohio-right")
        .expect("the second monitor is listed")
        .clone();
    assert_eq!((right.x, right.width), (640, 640));

    // The probe sits on the second monitor: at root (700, 100), which is (60, 100) of that monitor.
    let probe = Probe::start(&server.display, Some((700, 100)));
    assert_eq!((probe.x, probe.y), (700, 100));
    let frame = desktop
        .capture(&right.id, Some(Rect::new(60, 100, 128, 96)))
        .expect("a region of the second monitor");
    let (different, first) = differences(frame.rgb());
    assert_eq!(
        different, 0,
        "the second monitor's region is not the probe's pattern (first difference {first:?}): the monitor's offset was ignored"
    );
    drop(probe);

    let mut probe = Probe::start(&server.display, Some((700, 100)));
    let _pointer = Pointer::save(&desktop);
    desktop
        .input(&[InputEvent::Button {
            button: Button::Left,
            action: ButtonAction::Click,
            count: None,
            display: Some(right.id.clone()),
            x: Some(60 + 64),
            y: Some(100 + 48),
        }])
        .expect("a click on the second monitor");
    assert_eq!(
        probe.events_for(Duration::from_millis(500), 1),
        ["button 1 764 148"],
        "the click's root point is the monitor's offset plus its own point"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Through a node.

struct Peer {
    node: Arc<Node>,
    id: PeerId,
    keys: Arc<MemoryKeys>,
    grants: Arc<MemoryGrants>,
}

impl Peer {
    fn new(gui: GuiConfig, desktop: Option<Arc<X11Desktop>>) -> Arc<Self> {
        let config = NodeConfig {
            link: LinkConfig {
                recheck: Span(Duration::from_millis(400)),
                ..LinkConfig::default()
            },
            gui,
            ..NodeConfig::default()
        };
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let mut node =
            Node::new(config, identity, keys.clone(), grants.clone()).expect("the node is built");
        if let Some(desktop) = desktop {
            node = node.with_desktop(desktop);
        }
        Arc::new(Self {
            node: Arc::new(node),
            id,
            keys,
            grants,
        })
    }
}

struct World {
    _host: Arc<Peer>,
    link: PeerLink,
    channels: [Arc<StreamChannel>; 2],
    threads: Vec<JoinHandle<()>>,
}

impl Drop for World {
    fn drop(&mut self) {
        for channel in &self.channels {
            channel.close();
        }
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

fn world(desktop: Arc<X11Desktop>, grant: Grant) -> World {
    let gui = GuiConfig {
        enabled: true,
        ..GuiConfig::default()
    };
    let host = Peer::new(gui, Some(desktop));
    let client = Peer::new(GuiConfig::default(), None);
    host.keys.accept(client.id);
    host.grants.set(client.id, grant);
    client.keys.accept(host.id);
    let settings = Settings::default();
    let (one, two) = memory::pipe(1024 * 1024);
    let dial_end = Arc::new(stream(one, &settings).expect("a channel"));
    let answer_end = Arc::new(stream(two, &settings).expect("a channel"));
    let threads = vec![
        thread::spawn({
            let (peer, channel, expect) = (client.clone(), dial_end.clone(), host.id);
            move || {
                let _ = peer.node.connect_channel(&*channel, &expect, None);
                channel.close();
            }
        }),
        thread::spawn({
            let (peer, channel) = (host.clone(), answer_end.clone());
            move || {
                peer.node.serve_channel(&*channel, None);
                channel.close();
            }
        }),
    ];
    let until = Instant::now() + WAIT;
    let link = loop {
        if let Some(link) = client.node.peer(&host.id) {
            break link;
        }
        assert!(Instant::now() < until, "no link came up");
        thread::sleep(Duration::from_millis(20));
    };
    World {
        _host: host,
        link,
        channels: [dial_end, answer_end],
        threads,
    }
}

#[test]
fn a_node_serves_the_probe_window_and_names_the_x11_backend() {
    let _turn = turn();
    let Some(server) = server("a_node_serves_the_probe_window_and_names_the_x11_backend") else {
        return;
    };
    let desktop = Arc::new(desktop(&server));
    let expected = desktop.state().expect("a state");
    let grant = Grant::allowing([Permission::Info, Permission::GuiView]);
    let world = world(Arc::clone(&desktop), grant);
    let info = Control::open(&world.link)
        .unwrap()
        .info(Some(WAIT))
        .unwrap();
    assert!(
        info.gui.enabled && info.gui.available,
        "{:?}",
        info.gui.reason
    );
    assert_eq!(info.gui.backend.as_deref(), Some("x11"));
    assert!(info.gui.input && info.gui.clipboard);
    assert_eq!(info.gui.sessions.len(), 1);
    assert_eq!(info.gui.sessions[0].displays, expected.displays);

    let mut probe = Probe::start(&server.display, server.private.then_some((40, 30)));
    probe.placed_on(&expected);
    let (display, left, top) = display_of(&expected, probe.x, probe.y).expect("a display");
    let gui = Gui::open(&world.link, &GuiRequest::new([Want::View])).unwrap();
    let image = gui
        .snapshot(
            &Snapshot {
                display: Some(display.id),
                region: Some(Rect::new(
                    u32::try_from(left).unwrap(),
                    u32::try_from(top).unwrap(),
                    probe.width,
                    probe.height,
                )),
                format: Some(ImageFormat::Png),
                ..Snapshot::default()
            },
            Some(WAIT),
        )
        .expect("a snapshot of the probe's region");
    let decoded = image::load_from_memory(&image.bytes)
        .expect("a PNG")
        .to_rgb8();
    assert_eq!(decoded.dimensions(), (128, 96));
    assert_eq!(
        differences(decoded.as_raw()).0,
        0,
        "the snapshot is the probe's pattern"
    );
}
