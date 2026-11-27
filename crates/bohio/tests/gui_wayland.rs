//! The Wayland desktop backend against a compositor, through the desktop portals.
//!
//! Nothing here touches a compositor unless `BOHIO_TEST_WAYLAND=1`; without it the live tests print `SKIPPED` with the
//! reason. `tools/wayland_headless.sh` makes the session these tests run in: a headless sway whose one output,
//! `HEADLESS-1` (1280x720), is a solid colour (`BOHIO_TEST_WAYLAND_RGB`), PipeWire, WirePlumber, `xdg-desktop-portal` and
//! `xdg-desktop-portal-wlr` with a chooser that needs no click, all on a private D-Bus session and a private runtime
//! directory, so no display of the person running the tests is captured or driven.
//!
//! ```text
//! tools/wayland_headless.sh cargo test -p bohio --features gui-wayland --test gui_wayland
//! ```
//!
//! `xdg-desktop-portal-wlr` has ScreenCast and no RemoteDesktop, so these tests cover the view and the refusal of input;
//! the portal's input calls, its dialog and its restore token need a portal that has them (GNOME's), which is not run here.

#![cfg(all(target_os = "linux", feature = "gui-wayland"))]
#![forbid(unsafe_code)]

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{GuiConfig, LinkConfig, NodeConfig, Span};
use bohio::gui::{Desktop, WaylandDesktop, WaylandSettings};
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_client::control::Control;
use bohio_client::gui::{Gui, SnapshotOptions};
use bohio_client::link::{Settings, stream};
use bohio_proto::ErrorCode;
use bohio_proto::gui::{
    Button, ButtonAction, DisplayId, GuiRequest, ImageFormat, InputEvent, Rect, Want,
};
use cabuya::auth::{Identity, Method};
use coqui_peer::PeerId;
use coqui_transport::{Channel, StreamChannel, memory};

const WAIT: Duration = Duration::from_secs(30);

/// One live test at a time: they share a compositor and its one portal.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The colour the compositor's output is filled with, when a compositor is there to ask.
fn compositor(test: &str) -> Option<[u8; 3]> {
    if std::env::var("BOHIO_TEST_WAYLAND").as_deref() != Ok("1") {
        println!(
            "SKIPPED: {test}: BOHIO_TEST_WAYLAND=1 is not set (tools/wayland_headless.sh sets it), so no compositor is captured"
        );
        return None;
    }
    let hex = std::env::var("BOHIO_TEST_WAYLAND_RGB").unwrap_or_else(|_| "336699".to_owned());
    let value = u32::from_str_radix(&hex, 16).expect("BOHIO_TEST_WAYLAND_RGB is six hex digits");
    Some([
        u8::try_from(value >> 16 & 0xff).unwrap(),
        u8::try_from(value >> 8 & 0xff).unwrap(),
        u8::try_from(value & 0xff).unwrap(),
    ])
}

fn desktop() -> WaylandDesktop {
    WaylandDesktop::new(WaylandSettings {
        capture_wait: WAIT,
        start_wait: WAIT,
        ..WaylandSettings::default()
    })
}

/// How many pixels of `rgb` are not `colour`, and the first one that is not.
fn differences(rgb: &[u8], colour: [u8; 3]) -> (usize, Option<usize>) {
    let mut count = 0;
    let mut first = None;
    for (index, pixel) in rgb.chunks(3).enumerate() {
        if pixel != colour {
            count += 1;
            first.get_or_insert(index);
        }
    }
    (count, first)
}

/// The whole first display, once the compositor has painted its background (the bar of the first frame can be black).
fn painted(desktop: &WaylandDesktop, colour: [u8; 3]) -> (bohio::gui::DesktopState, Vec<u8>) {
    // The compositor sends a picture when the screen changes, and this screen does not: the harness's poke paints the
    // background black and back, so it is the colour again when the poke is over.
    let poke = std::env::var("BOHIO_TEST_WAYLAND_POKE").ok();
    let run_poke = |command: &str| {
        let _ = Command::new("sh")
            .args(["-c", command])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };
    // Until the first picture is there, poke again and again.
    let stop = Arc::new(AtomicBool::new(false));
    let poker = poke.clone().map(|command| {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                run_poke(&command);
                thread::sleep(Duration::from_millis(700));
            }
        })
    });
    let state = desktop.state().expect("a state");
    let display = state.displays[0].id.clone();
    let first = desktop.capture(&display, None);
    stop.store(true, Ordering::SeqCst);
    if let Some(poker) = poker {
        poker.join().expect("the poke thread");
    }
    let mut frame = first.expect("a first picture");
    // A picture sent while the compositor renegotiates the stream can be lost, and nothing sends it again: poke until the
    // latest picture is the colour, each time after the pictures of the last poke have had time to arrive.
    let until = Instant::now() + WAIT;
    loop {
        thread::sleep(Duration::from_millis(600));
        frame = desktop.capture(&display, None).unwrap_or(frame);
        if differences(frame.rgb(), colour).0 == 0 || Instant::now() >= until {
            return (state, frame.into_rgb());
        }
        if let Some(command) = &poke {
            run_poke(command);
        }
    }
}

#[test]
fn the_compositors_output_is_one_display_and_its_picture_is_exact() {
    let _turn = turn();
    let Some(colour) = compositor("the_compositors_output_is_one_display_and_its_picture_is_exact")
    else {
        return;
    };
    let desktop = desktop();
    let (state, rgb) = painted(&desktop, colour);
    assert_eq!(state.displays.len(), 1, "{:?}", state.displays);
    let display = &state.displays[0];
    assert!(display.primary);
    assert_eq!((display.width, display.height), (1280, 720));
    assert!((display.scale - 1.0).abs() < f64::EPSILON, "{display:?}");
    assert!(state.active);
    assert_eq!(rgb.len(), 1280 * 720 * 3);
    let (wrong, first) = differences(&rgb, colour);
    assert_eq!(
        wrong,
        0,
        "the first wrong pixel is at {first:?}; the first pixel is {:?}",
        &rgb[..3]
    );

    // A region is that part of the picture.
    let part = desktop
        .capture(&display.id, Some(Rect::new(1200, 700, 80, 20)))
        .expect("a region");
    assert_eq!((part.width(), part.height()), (80, 20));
    assert_eq!(differences(part.rgb(), colour).0, 0);
    let error = desktop
        .capture(&DisplayId("nothing".to_owned()), None)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
}

#[test]
fn without_a_remote_desktop_portal_it_is_a_view_and_input_is_unsupported() {
    let _turn = turn();
    let Some(colour) =
        compositor("without_a_remote_desktop_portal_it_is_a_view_and_input_is_unsupported")
    else {
        return;
    };
    let desktop = desktop();
    painted(&desktop, colour);
    let capabilities = desktop.capabilities();
    assert_eq!(capabilities.backend, "wayland");
    assert!(
        !capabilities.input,
        "xdg-desktop-portal-wlr has no RemoteDesktop"
    );
    assert!(!capabilities.clipboard);
    let error = desktop
        .input(&[InputEvent::Button {
            button: Button::Left,
            action: ButtonAction::Click,
            count: None,
            display: None,
            x: Some(10),
            y: Some(10),
        }])
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unsupported);
    assert!(error.message.contains("RemoteDesktop"), "{error}");
    assert_eq!(
        desktop.clipboard_text().unwrap_err().code,
        ErrorCode::Unsupported
    );
    assert!(desktop.cursor().is_none());
}

#[test]
fn a_session_the_portal_ends_is_started_again_by_the_next_request() {
    let _turn = turn();
    let Some(colour) = compositor("a_session_the_portal_ends_is_started_again_by_the_next_request")
    else {
        return;
    };
    // Two desktops one after the other: the first one's session is closed when it is dropped (its PipeWire thread
    // ends, its portal session closes), and the second makes its own.
    for _ in 0..2 {
        let desktop = desktop();
        let (_, rgb) = painted(&desktop, colour);
        assert_eq!(differences(&rgb, colour).0, 0);
    }
}

#[test]
fn the_portal_session_closes_after_idle_and_the_next_request_opens_it_again() {
    let _turn = turn();
    let Some(colour) =
        compositor("the_portal_session_closes_after_idle_and_the_next_request_opens_it_again")
    else {
        return;
    };
    let desktop = WaylandDesktop::new(WaylandSettings {
        capture_wait: WAIT,
        start_wait: WAIT,
        idle: Duration::from_secs(2),
        ..WaylandSettings::default()
    });
    let (_, rgb) = painted(&desktop, colour);
    assert_eq!(differences(&rgb, colour).0, 0);
    assert!(desktop.session_open());
    let until = Instant::now() + WAIT;
    while desktop.session_open() {
        assert!(Instant::now() < until, "the session was never closed");
        thread::sleep(Duration::from_millis(200));
    }
    let (_, again) = painted(&desktop, colour);
    assert_eq!(differences(&again, colour).0, 0);
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
    fn new(gui: GuiConfig, desktop: Option<Arc<WaylandDesktop>>) -> Arc<Self> {
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

fn world(desktop: Arc<WaylandDesktop>, grant: Grant) -> World {
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
fn a_node_serves_the_compositors_picture_and_names_the_wayland_backend() {
    let _turn = turn();
    let Some(colour) =
        compositor("a_node_serves_the_compositors_picture_and_names_the_wayland_backend")
    else {
        return;
    };
    let desktop = Arc::new(desktop());
    let (expected, _) = painted(&desktop, colour);
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
    assert_eq!(info.gui.backend.as_deref(), Some("wayland"));
    assert!(!info.gui.input, "a view only");
    assert!(!info.gui.clipboard);
    assert_eq!(info.gui.sessions.len(), 1);
    assert_eq!(info.gui.sessions[0].displays, expected.displays);

    let gui = Gui::open(&world.link, &GuiRequest::new([Want::View])).unwrap();
    let image = gui
        .snapshot(
            &SnapshotOptions {
                display: Some(expected.displays[0].id.clone()),
                region: Some(Rect::new(0, 0, 1280, 720)),
                format: Some(ImageFormat::Png),
                ..SnapshotOptions::default()
            },
            Some(WAIT),
        )
        .expect("a snapshot");
    let decoded = image::load_from_memory(&image.bytes)
        .expect("a PNG")
        .to_rgb8();
    assert_eq!(decoded.dimensions(), (1280, 720));
    assert_eq!(
        differences(decoded.as_raw(), colour).0,
        0,
        "the snapshot is the output's colour"
    );
}
