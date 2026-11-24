//! A service account's own desktop: `[source.native] own_session`, against the user units of
//! `packaging/systemd/own-desktop/`.
//!
//! The refusals need no desktop. The live test needs the own desktop up, which `tools/own_desktop_units.sh run` makes
//! with the units as shipped (renamed so they cannot meet a unit of the account's own) and removes afterwards:
//!
//! ```text
//! cargo build -p bohio --example gui_probe_x11
//! tools/own_desktop_units.sh run cargo test -p bohio --test gui_own_desktop
//! ```
//!
//! Without `BOHIO_TEST_OWN_DESKTOP=1` the live test prints `SKIPPED`. It serves the display `:UID` of the account that runs
//! it, so the probe window it paints and drives is the only thing it touches.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

#[path = "../examples/support/x11_pattern.rs"]
mod pattern;

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{GuiConfig, LinkConfig, NodeConfig, SourceTables, Span};
use bohio::gui::{own_display, own_uid};
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_client::control::Control;
use bohio_client::gui::{Gui, SnapshotOptions};
use bohio_client::link::{Settings, stream};
use bohio_proto::gui::{
    Button, ButtonAction, GuiRequest, ImageFormat, InputEvent, KeyAction, Rect, Want,
};
use coqui_link::auth::{Identity, Method};
use coqui_peer::PeerId;
use coqui_transport::{Channel, StreamChannel, memory};

const WAIT: Duration = Duration::from_secs(30);

/// One test at a time: the live one shares the account's display and its one pointer.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn tables(text: &str) -> SourceTables {
    #[derive(serde::Deserialize)]
    struct Whole {
        source: SourceTables,
    }
    toml::from_str::<Whole>(text).expect("source tables").source
}

// ---------------------------------------------------------------------------------------------------------------
// A node over an in-memory link, serving what `[source.native]` says.

struct Peer {
    node: Arc<Node>,
    id: PeerId,
    keys: Arc<MemoryKeys>,
    grants: Arc<MemoryGrants>,
}

impl Peer {
    fn new(gui: GuiConfig, source: SourceTables) -> Arc<Self> {
        let config = NodeConfig {
            link: LinkConfig {
                recheck: Span(Duration::from_millis(400)),
                ..LinkConfig::default()
            },
            gui,
            source,
            ..NodeConfig::default()
        };
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let node =
            Node::new(config, identity, keys.clone(), grants.clone()).expect("the node is built");
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

fn world(native: &str, grant: Grant) -> World {
    let gui = GuiConfig {
        enabled: true,
        ..GuiConfig::default()
    };
    let host = Peer::new(gui, tables(&format!("[source.native]\n{native}\n")));
    let client = Peer::new(GuiConfig::default(), SourceTables::default());
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

fn info_gui(world: &World) -> bohio_proto::gui::GuiInfo {
    Control::open(&world.link)
        .unwrap()
        .info(Some(WAIT))
        .unwrap()
        .gui
}

fn view_grant() -> Grant {
    Grant::allowing([Permission::Info, Permission::GuiView])
}

// ---------------------------------------------------------------------------------------------------------------
// What needs no desktop: the runtime directory is the account's, or the node says why not.

/// A directory of this account with `mode`, which is removed with the value.
fn directory(mode: u32) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a directory");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).expect("its mode");
    dir
}

#[test]
fn a_runtime_directory_others_can_enter_is_refused_naming_the_path() {
    let _turn = turn();
    for mode in [0o755, 0o750, 0o770, 0o777] {
        let dir = directory(mode);
        let path = dir.path().display().to_string();
        let world = world(
            &format!("own_session = \"x11\"\nown_runtime_dir = {path:?}"),
            view_grant(),
        );
        let gui = info_gui(&world);
        assert!(gui.enabled && !gui.available, "{gui:?}");
        let reason = gui.reason.expect("a reason");
        assert!(reason.contains(&path), "{reason}");
        assert!(reason.contains(&format!("{mode:04o}")), "{reason}");
        assert!(reason.contains("0700"), "{reason}");
    }
}

#[test]
fn a_runtime_directory_of_another_account_is_refused_naming_the_path() {
    let _turn = turn();
    if own_uid().expect("a uid") == 0 {
        println!("SKIPPED: this test runs as root, whose own directories are root's");
        return;
    }
    // `/` belongs to root, and every account but root is another account.
    let world = world(
        "own_session = \"x11\"\nown_runtime_dir = \"/\"",
        view_grant(),
    );
    let gui = info_gui(&world);
    assert!(!gui.available, "{gui:?}");
    let reason = gui.reason.expect("a reason");
    assert!(reason.contains("/ belongs to uid 0"), "{reason}");
    assert!(reason.contains("own_session"), "{reason}");
}

#[test]
fn a_missing_runtime_directory_says_what_starts_it() {
    let _turn = turn();
    let dir = directory(0o700);
    let missing = dir.path().join("not-there").display().to_string();
    let world = world(
        &format!("own_session = \"x11\"\nown_runtime_dir = {missing:?}"),
        view_grant(),
    );
    let reason = info_gui(&world).reason.expect("a reason");
    assert!(reason.contains(&missing), "{reason}");
    assert!(reason.contains("enable-linger"), "{reason}");
}

#[test]
fn an_account_whose_desktop_is_not_up_is_unavailable_naming_the_display_and_the_unit() {
    let _turn = turn();
    let dir = directory(0o700);
    let path = dir.path().display().to_string();
    // The runtime directory is the account's, and the display (named by the file, which wins over the one derived from the
    // uid) is not there.
    let world = world(
        &format!("own_session = \"x11\"\nown_runtime_dir = {path:?}\nx11_display = \":4999\""),
        view_grant(),
    );
    let gui = info_gui(&world);
    assert!(gui.enabled && !gui.available, "{gui:?}");
    let reason = gui.reason.expect("a reason");
    assert!(reason.contains(":4999"), "{reason}");
    assert!(reason.contains("bohio-desktop.target"), "{reason}");
    assert!(reason.contains("bohio-xwayland.service"), "{reason}");
}

#[test]
fn an_own_session_the_file_misspells_is_refused() {
    let _turn = turn();
    let gui = GuiConfig {
        enabled: true,
        ..GuiConfig::default()
    };
    let source = tables("[source.native]\nown_session = \"x12\"\n");
    let config = NodeConfig {
        gui,
        source,
        ..NodeConfig::default()
    };
    let identity = Identity::generate(Method::DEFAULT).expect("an identity");
    let refused = Node::new(
        config,
        identity,
        Arc::new(MemoryKeys::new()),
        Arc::new(MemoryGrants::new()),
    );
    let Err(error) = refused else {
        panic!("a node with a misspelled own_session is not built");
    };
    assert!(error.to_string().contains("own_session"), "{error}");
}

// ---------------------------------------------------------------------------------------------------------------
// The own desktop, up.

fn live(test: &str) -> bool {
    if std::env::var("BOHIO_TEST_OWN_DESKTOP").as_deref() == Ok("1") {
        return true;
    }
    println!(
        "SKIPPED: {test}: BOHIO_TEST_OWN_DESKTOP=1 is not set (tools/own_desktop_units.sh run sets it), so no desktop is served"
    );
    false
}

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

/// The probe window: a known pattern, and what it receives.
struct Probe {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    reader: Option<JoinHandle<()>>,
    x: i32,
    y: i32,
    seen: Vec<String>,
}

impl Probe {
    fn start(display: &str, at: (i32, i32)) -> Self {
        let mut child = Command::new(probe_path())
            .arg(display)
            .arg(at.0.to_string())
            .arg(at.1.to_string())
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
            seen: Vec::new(),
        };
        let ready = probe
            .wait_for(|line| line.starts_with("ready "), WAIT)
            .expect("the probe reports its window");
        let numbers: Vec<i32> = ready
            .split_whitespace()
            .skip(1)
            .filter_map(|word| word.parse().ok())
            .collect();
        probe.x = numbers[0];
        probe.y = numbers[1];
        probe
    }

    fn wait_for(&mut self, matches: impl Fn(&str) -> bool, within: Duration) -> Option<String> {
        let until = Instant::now() + within;
        loop {
            let left = until.checked_duration_since(Instant::now())?;
            let line = self.lines.recv_timeout(left).ok()?;
            self.seen.push(line.clone());
            if matches(&line) {
                return Some(line);
            }
        }
    }

    /// The `key` and `button` lines from now until nothing more comes for `quiet`, once `at_least` have.
    fn events_for(&mut self, quiet: Duration, at_least: usize) -> Vec<String> {
        let mut found = Vec::new();
        let until = Instant::now() + Duration::from_secs(10);
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
        let last = self
            .seen
            .iter()
            .rev()
            .find(|line| *line == "focused" || *line == "unfocused")
            .cloned();
        if last.as_deref() == Some("focused") {
            return true;
        }
        self.wait_for(|line| line == "focused", within).is_some()
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

fn differences(got: &[u8]) -> usize {
    let want = pattern::rgb();
    got.chunks(3)
        .zip(want.chunks(3))
        .filter(|(a, b)| a != b)
        .count()
}

#[test]
fn an_own_desktop_serves_a_snapshot_and_input_to_a_peer() {
    let _turn = turn();
    if !live("an_own_desktop_serves_a_snapshot_and_input_to_a_peer") {
        return;
    }
    let uid = own_uid().expect("a uid");
    let display = own_display(uid);
    let grant = Grant::allowing([Permission::Info, Permission::GuiView, Permission::GuiInput]);
    // Nothing else is set: the runtime directory is /run/user/UID and the display is :UID, both from the node's own uid.
    let world = world("own_session = \"x11\"", grant);

    let info = info_gui(&world);
    assert!(info.enabled && info.available, "{:?}", info.reason);
    assert_eq!(info.backend.as_deref(), Some("x11"));
    assert!(info.input, "XTest input is offered");
    assert!(
        !info.lock_detection,
        "a headless desktop has no lock screen to detect"
    );
    assert_eq!(info.sessions.len(), 1);
    assert_eq!(
        info.sessions[0].locked,
        Some(false),
        "and it is never locked"
    );
    let shown = info.sessions[0].displays[0].clone();
    println!(
        "OWN DESKTOP {display}: {}x{} at {},{}",
        shown.width, shown.height, shown.x, shown.y
    );

    let mut probe = Probe::start(&display, (40, 30));
    let left = probe.x - shown.x;
    let top = probe.y - shown.y;
    let gui = Gui::open(&world.link, &GuiRequest::new([Want::View, Want::Input])).unwrap();

    // The snapshot is the probe's pattern, pixel for pixel: the root of the own desktop is readable.
    let image = gui
        .snapshot(
            &SnapshotOptions {
                display: Some(shown.id.clone()),
                region: Some(Rect::new(
                    u32::try_from(left).unwrap(),
                    u32::try_from(top).unwrap(),
                    u32::from(pattern::WIDTH),
                    u32::from(pattern::HEIGHT),
                )),
                format: Some(ImageFormat::Png),
                ..SnapshotOptions::default()
            },
            Some(WAIT),
        )
        .expect("a snapshot of the probe's region");
    let decoded = image::load_from_memory(&image.bytes)
        .expect("a PNG")
        .to_rgb8();
    assert_eq!(
        decoded.dimensions(),
        (u32::from(pattern::WIDTH), u32::from(pattern::HEIGHT))
    );
    assert_eq!(differences(decoded.as_raw()), 0, "the probe's pattern");

    // Input: a click at the probe's centre arrives at its root point; typed text arrives as keys.
    let (cx, cy) = (left + 64, top + 48);
    let (rx, ry) = (probe.x + 64, probe.y + 48);
    let _ = probe.focused(Duration::from_secs(3));
    let mut arrived = Vec::new();
    for _ in 0..10 {
        gui.input(
            &[
                InputEvent::Move {
                    display: Some(shown.id.clone()),
                    x: cx,
                    y: cy,
                },
                InputEvent::Button {
                    button: Button::Left,
                    action: ButtonAction::Click,
                    count: None,
                    display: Some(shown.id.clone()),
                    x: Some(cx),
                    y: Some(cy),
                },
            ],
            Some(WAIT),
        )
        .expect("a click is sent");
        arrived = probe.events_for(Duration::from_millis(500), 1);
        if !arrived.is_empty() {
            break;
        }
    }
    assert_eq!(
        arrived,
        [format!("button 1 {rx} {ry}")],
        "the click arrived at its root point (the probe saw {:?})",
        probe.seen
    );
    assert!(
        probe.focused(Duration::from_secs(3)),
        "the probe has the keyboard focus (seen {:?})",
        probe.seen
    );
    gui.input(
        &[InputEvent::Text {
            text: "aBé".to_owned(),
        }],
        Some(WAIT),
    )
    .expect("text is typed");
    assert_eq!(
        probe.events_for(Duration::from_millis(500), 4),
        ["key U+0061", "key keysym:FFE1", "key U+0042", "key U+00E9"],
        "ASCII, a shifted capital and a character the keyboard has no key for"
    );
    gui.input(
        &[
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
        ],
        Some(WAIT),
    )
    .expect("a combination is sent");
    assert_eq!(
        probe.events_for(Duration::from_millis(500), 2),
        ["key keysym:FFE3", "key U+0061"]
    );
}
