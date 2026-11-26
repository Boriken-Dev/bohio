//! `bohio-gui/1` served by a node over memory pipes to a peer, against a painted in-memory desktop: no screen is touched.

#![forbid(unsafe_code)]

#[path = "../examples/support/painted.rs"]
mod painted;

use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{GuiConfig, LinkConfig, NodeConfig, Span};
use bohio::link::GuiChange;
use bohio::node::{MemoryGrants, MemoryKeys, Node, NodeEvent, NodeEvents, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_client::Error;
use bohio_client::control::Control;
use bohio_client::gui::{Gui, SnapshotOptions};
use bohio_client::link::{Settings, stream};
use bohio_proto::gui::{
    Button, ButtonAction, DisplayId, GuiRequest, ImageFormat, InputEvent, KeyAction, Rect, Settle,
    SettleOptions, Want,
};
use bohio_proto::{ErrorBody, ErrorCode};
use cabuya::auth::{Identity, Method};
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure, StreamChannel, memory};
use painted::{CLICK, MARK, Painted, pattern};

const WAIT: Duration = Duration::from_secs(30);
const RECHECK: Duration = Duration::from_millis(400);

#[derive(Default)]
struct Recorder(Mutex<Vec<NodeEvent>>);

impl NodeEvents for Recorder {
    fn event(&self, event: &NodeEvent) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event.clone());
    }
}

struct Peer {
    node: Arc<Node>,
    id: PeerId,
    keys: Arc<MemoryKeys>,
    grants: Arc<MemoryGrants>,
    events: Arc<Recorder>,
}

impl Peer {
    fn new(gui: GuiConfig, desktop: Option<Arc<Painted>>) -> Arc<Self> {
        let config = NodeConfig {
            link: LinkConfig {
                recheck: Span(RECHECK),
                ..LinkConfig::default()
            },
            gui,
            ..NodeConfig::default()
        };
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let events = Arc::new(Recorder::default());
        let mut node = Node::new(config, identity, keys.clone(), grants.clone())
            .expect("the node is built")
            .with_events(events.clone());
        if let Some(desktop) = desktop {
            node = node.with_desktop(desktop);
        }
        Arc::new(Self {
            node: Arc::new(node),
            id,
            keys,
            grants,
            events,
        })
    }
}

struct Wire {
    channels: [Arc<StreamChannel>; 2],
    threads: Vec<JoinHandle<()>>,
}

impl Wire {
    fn make(dialer: &Arc<Peer>, answerer: &Arc<Peer>) -> Self {
        let settings = Settings::default();
        let (one, two) = memory::pipe(1024 * 1024);
        let dial_end = Arc::new(stream(one, &settings).expect("a channel"));
        let answer_end = Arc::new(stream(two, &settings).expect("a channel"));
        let mut threads = Vec::new();
        threads.push(thread::spawn({
            let (peer, channel, expect) = (dialer.clone(), dial_end.clone(), answerer.id);
            move || {
                let _ = peer.node.connect_channel(&*channel, &expect, None);
                channel.close();
            }
        }));
        threads.push(thread::spawn({
            let (peer, channel) = (answerer.clone(), answer_end.clone());
            move || {
                peer.node.serve_channel(&*channel, None);
                channel.close();
            }
        }));
        Self {
            channels: [dial_end, answer_end],
            threads,
        }
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        for channel in &self.channels {
            channel.close();
        }
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

/// A host with a painted desktop and one client peer granted `grant`, linked.
struct World {
    desktop: Arc<Painted>,
    host: Arc<Peer>,
    client: Arc<Peer>,
    link: PeerLink,
    _wire: Wire,
}

fn gui_config() -> GuiConfig {
    GuiConfig {
        enabled: true,
        // The presence notice is a log line here; its own tests are in gui_presence.
        ..GuiConfig::default()
    }
}

fn world(config: GuiConfig, grant: Grant) -> World {
    let desktop = Arc::new(Painted::new());
    let host = Peer::new(config, Some(desktop.clone()));
    let client = Peer::new(GuiConfig::default(), None);
    host.keys.accept(client.id);
    host.grants.set(client.id, grant);
    client.keys.accept(host.id);
    let wire = Wire::make(&client, &host);
    let until = Instant::now() + WAIT;
    let link = loop {
        if let Some(link) = client.node.peer(&host.id) {
            break link;
        }
        assert!(Instant::now() < until, "no link came up");
        thread::sleep(Duration::from_millis(20));
    };
    World {
        desktop,
        host,
        client,
        link,
        _wire: wire,
    }
}

fn all() -> Grant {
    Grant::allowing([
        Permission::Info,
        Permission::GuiView,
        Permission::GuiInput,
        Permission::GuiClipboard,
    ])
}

fn open(world: &World, want: &[Want]) -> Gui {
    Gui::open(&world.link, &GuiRequest::new(want.iter().copied())).expect("the stream opens")
}

fn remote(error: &Error) -> &ErrorBody {
    match error {
        Error::Remote(body) => body,
        other => panic!("expected the host's error, got {other:?}"),
    }
}

fn decode_rgb(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let image = image::load_from_memory(bytes).expect("an image").to_rgb8();
    (image.width(), image.height(), image.into_raw())
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let sum: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
        .sum();
    if sum == 0.0 {
        return 99.0;
    }
    10.0 * (255.0_f64 * 255.0 / (sum / a.len() as f64)).log10()
}

#[test]
fn a_png_snapshot_is_the_painted_pixels_exactly_and_a_jpeg_is_close() {
    let world = world(gui_config(), all());
    let gui = open(&world, &[Want::View]);
    assert_eq!(gui.state().displays.len(), 2);
    let png = gui
        .snapshot(
            &SnapshotOptions {
                format: Some(ImageFormat::Png),
                ..SnapshotOptions::default()
            },
            Some(WAIT),
        )
        .unwrap();
    let (width, height, pixels) = decode_rgb(&png.bytes);
    assert_eq!((width, height), (320, 200));
    assert_eq!(pixels, world.desktop.pixels(0).2);
    assert_eq!(png.header.scale, 1.0);
    assert_eq!(png.header.length, png.bytes.len() as u64);
    assert_eq!(pixels[0..3], pattern(0, 0, 0));
    let jpeg = gui
        .snapshot(
            &SnapshotOptions {
                format: Some(ImageFormat::Jpeg),
                quality: Some(90),
                ..SnapshotOptions::default()
            },
            Some(WAIT),
        )
        .unwrap();
    let (_, _, lossy) = decode_rgb(&jpeg.bytes);
    assert!(
        psnr(&lossy, &pixels) >= 30.0,
        "psnr {}",
        psnr(&lossy, &pixels)
    );
    let webp = gui
        .snapshot(
            &SnapshotOptions {
                format: Some(ImageFormat::Webp),
                ..SnapshotOptions::default()
            },
            Some(WAIT),
        )
        .unwrap();
    let (_, _, exact) = decode_rgb(&webp.bytes);
    assert_eq!(exact, pixels);
}

#[test]
fn fitting_reports_the_scale_and_a_region_and_another_display_are_served() {
    let world = world(gui_config(), all());
    let gui = open(&world, &[Want::View]);
    let small = gui
        .snapshot(
            &SnapshotOptions {
                max_width: Some(160),
                max_height: Some(160),
                ..SnapshotOptions::default()
            },
            Some(WAIT),
        )
        .unwrap();
    assert_eq!((small.header.width, small.header.height), (160, 100));
    assert!((small.header.scale - 0.5).abs() < 1e-9);
    assert_eq!(decode_rgb(&small.bytes).0, 160);
    let region = gui
        .snapshot(
            &SnapshotOptions {
                region: Some(Rect::new(10, 20, 30, 40)),
                ..SnapshotOptions::default()
            },
            Some(WAIT),
        )
        .unwrap();
    let (width, height, pixels) = decode_rgb(&region.bytes);
    assert_eq!((width, height), (30, 40));
    assert_eq!(pixels[0..3], pattern(0, 10, 20));
    assert_eq!(region.header.region, Rect::new(10, 20, 30, 40));
    let second = gui
        .snapshot(
            &SnapshotOptions {
                display: Some(DisplayId::from("1")),
                ..SnapshotOptions::default()
            },
            Some(WAIT),
        )
        .unwrap();
    assert_eq!((second.header.width, second.header.height), (160, 100));
    let outside = gui
        .snapshot(
            &SnapshotOptions {
                region: Some(Rect::new(300, 0, 100, 10)),
                ..SnapshotOptions::default()
            },
            Some(WAIT),
        )
        .unwrap_err();
    assert_eq!(remote(&outside).code, ErrorCode::BadRequest);
    // The stream goes on after a request that failed.
    assert!(
        gui.snapshot(&SnapshotOptions::default(), Some(WAIT))
            .is_ok()
    );
}

#[test]
fn a_click_paints_where_it_lands_and_text_and_a_combination_arrive_in_order() {
    let world = world(gui_config(), all());
    let gui = open(&world, &[Want::View, Want::Input]);
    let click = InputEvent::Button {
        button: Button::Left,
        action: ButtonAction::Click,
        count: None,
        display: None,
        x: Some(100),
        y: Some(50),
    };
    assert_eq!(gui.input(&[click], Some(WAIT)).unwrap(), 1);
    let image = gui
        .snapshot(&SnapshotOptions::default(), Some(WAIT))
        .unwrap();
    let (width, _, pixels) = decode_rgb(&image.bytes);
    let at = |x: u32, y: u32| (y * width + x) as usize * 3;
    assert_eq!(pixels[at(100, 50)..at(100, 50) + 3], CLICK);
    assert_eq!(pixels[at(100 + MARK - 1, 50 + MARK - 1)..][..3], CLICK);
    assert_eq!(
        pixels[at(100 + MARK, 50)..][..3],
        pattern(0, 100 + MARK, 50)
    );
    let mut events = vec![InputEvent::Text {
        text: "hello".into(),
    }];
    events.extend(InputEvent::combo("ctrl+shift+t").unwrap());
    events.push(InputEvent::Key {
        key: "enter".into(),
        action: KeyAction::Click,
    });
    gui.input(&events, Some(WAIT)).unwrap();
    let log = world.desktop.log();
    assert_eq!(
        &log[log.len() - 7..],
        [
            "text:hello",
            "key:control:press",
            "key:shift:press",
            "key:t:click",
            "key:shift:release",
            "key:control:release",
            "key:enter:click",
        ]
    );
    let bad = gui
        .input(
            &[InputEvent::Key {
                key: "ctrl+s".into(),
                action: KeyAction::Click,
            }],
            Some(WAIT),
        )
        .unwrap_err();
    assert_eq!(remote(&bad).code, ErrorCode::BadRequest);
}

#[test]
fn without_gui_input_nothing_reaches_the_desktop() {
    let world = world(
        gui_config(),
        Grant::allowing([Permission::GuiView, Permission::GuiClipboard]),
    );
    // Asking to control is refused by the link before the engine sees it.
    let refused = Gui::open(&world.link, &GuiRequest::new([Want::View, Want::Input])).unwrap_err();
    assert!(
        matches!(refused, Error::Transport(Failure::Permission)),
        "{refused:?}"
    );
    let gui = open(&world, &[Want::View]);
    let calls = world.desktop.calls();
    let denied = gui
        .input(&[InputEvent::Text { text: "x".into() }], Some(WAIT))
        .unwrap_err();
    assert_eq!(remote(&denied).code, ErrorCode::Denied);
    assert!(world.desktop.log().is_empty());
    assert_eq!(
        world.desktop.calls(),
        calls,
        "a denied input reached the desktop"
    );
}

#[test]
fn a_missing_clipboard_grant_refuses_clipboard_calls_and_a_held_one_works() {
    let world = world(
        gui_config(),
        Grant::allowing([Permission::GuiView, Permission::GuiInput]),
    );
    assert!(Gui::open(&world.link, &GuiRequest::new([Want::View, Want::Clipboard])).is_err());
    let gui = open(&world, &[Want::View]);
    let refused = gui.clipboard(Some(WAIT)).unwrap_err();
    assert_eq!(remote(&refused).code, ErrorCode::Denied);
    let full = self::world(gui_config(), all());
    let gui = open(&full, &[Want::View, Want::Clipboard]);
    gui.set_clipboard("copied text", Some(WAIT)).unwrap();
    assert_eq!(full.desktop.clipboard(), "copied text");
    assert_eq!(gui.clipboard(Some(WAIT)).unwrap(), "copied text");
}

#[test]
fn removing_gui_input_refuses_the_next_frame_at_once_and_ends_the_stream_as_revoked() {
    let world = world(gui_config(), all());
    let gui = open(&world, &[Want::View, Want::Input]);
    gui.input(&[InputEvent::Text { text: "a".into() }], Some(WAIT))
        .unwrap();
    world
        .host
        .grants
        .set(world.client.id, Grant::allowing([Permission::GuiView]));
    // Before the re-check: the frame is decided by the grant as it is now.
    let late = gui
        .input(&[InputEvent::Text { text: "b".into() }], Some(WAIT))
        .unwrap_err();
    assert_eq!(remote(&late).code, ErrorCode::Denied);
    assert_eq!(world.desktop.log(), ["text:a"]);
    let started = Instant::now();
    let ended = loop {
        match gui.snapshot(
            &SnapshotOptions::default(),
            Some(Duration::from_millis(200)),
        ) {
            Err(Error::Timeout) | Ok(_) => {}
            Err(error) => break error,
        }
        assert!(started.elapsed() < WAIT, "the stream was never ended");
        thread::sleep(Duration::from_millis(50));
    };
    assert!(
        matches!(ended, Error::Transport(Failure::Permission)),
        "{ended:?}"
    );
    assert!(started.elapsed() < RECHECK * 2 + Duration::from_secs(3));
}

#[test]
fn a_locked_screen_refuses_input_unless_the_host_allows_it() {
    let world = world(gui_config(), all());
    world.desktop.set_locked(Some(true));
    let gui = open(&world, &[Want::View, Want::Input]);
    let refused = gui
        .input(&[InputEvent::Text { text: "pw".into() }], Some(WAIT))
        .unwrap_err();
    assert_eq!(remote(&refused).code, ErrorCode::Unavailable);
    assert!(world.desktop.log().is_empty());
    assert!(
        gui.snapshot(&SnapshotOptions::default(), Some(WAIT))
            .is_ok()
    );
    let open_host = self::world(
        GuiConfig {
            input_when_locked: true,
            ..gui_config()
        },
        all(),
    );
    open_host.desktop.set_locked(Some(true));
    let gui = open(&open_host, &[Want::Input]);
    assert_eq!(
        gui.input(&[InputEvent::Text { text: "pw".into() }], Some(WAIT))
            .unwrap(),
        1
    );
    open_host.desktop.set_active(false);
    let away = gui
        .input(&[InputEvent::Text { text: "x".into() }], Some(WAIT))
        .unwrap_err();
    assert_eq!(remote(&away).code, ErrorCode::Unavailable);
}

#[test]
fn the_rates_hold() {
    let config = GuiConfig {
        max_input_rate: 5,
        snapshots_per_minute: 3,
        max_batch_events: 4,
        ..gui_config()
    };
    let world = world(config, all());
    let gui = open(&world, &[Want::View, Want::Input]);
    let key = |text: &str| InputEvent::Text {
        text: text.to_owned(),
    };
    gui.input(&[key("a"), key("b"), key("c"), key("d")], Some(WAIT))
        .unwrap();
    gui.input(&[key("e")], Some(WAIT)).unwrap();
    let busy = gui.input(&[key("f")], Some(WAIT)).unwrap_err();
    assert_eq!(remote(&busy).code, ErrorCode::Busy);
    assert_eq!(
        world.desktop.log().len(),
        5,
        "a refused batch applied something"
    );
    let long = gui
        .input(&(0..5).map(|_| key("1")).collect::<Vec<_>>(), Some(WAIT))
        .unwrap_err();
    assert_eq!(remote(&long).code, ErrorCode::BadRequest);
    for _ in 0..3 {
        gui.snapshot(&SnapshotOptions::default(), Some(WAIT))
            .unwrap();
    }
    let limited = gui
        .snapshot(&SnapshotOptions::default(), Some(WAIT))
        .unwrap_err();
    assert_eq!(remote(&limited).code, ErrorCode::Busy);
}

#[test]
fn settle_answers_true_once_painting_stops_and_false_at_the_timeout() {
    let config = GuiConfig {
        settle_interval: Span(Duration::from_millis(20)),
        settle_quiet: Span(Duration::from_millis(150)),
        settle_timeout: Span(Duration::from_millis(900)),
        settle_ignore_tiles: 0,
        ..gui_config()
    };
    let world = world(config, all());
    let gui = open(&world, &[Want::View]);
    let quiet = gui.settle(&Settle::default(), Some(WAIT)).unwrap();
    assert!(quiet.settled);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let painter = thread::spawn({
        let (desktop, stop) = (world.desktop.clone(), stop.clone());
        move || {
            let mut shade = 0_u8;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                shade = shade.wrapping_add(1);
                desktop.paint(0, Rect::new(0, 0, 8, 8), [shade, 0, 0]);
                thread::sleep(Duration::from_millis(10));
            }
        }
    });
    let moving = gui.settle(&Settle::default(), Some(WAIT)).unwrap();
    assert!(!moving.settled);
    assert!(moving.waited_ms >= 800);
    let with_snapshot = gui
        .snapshot(
            &SnapshotOptions {
                settle: Some(SettleOptions {
                    quiet_ms: Some(100),
                    timeout_ms: Some(300),
                }),
                ..SnapshotOptions::default()
            },
            Some(WAIT),
        )
        .unwrap();
    assert_eq!(with_snapshot.header.settled.map(|s| s.settled), Some(false));
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    painter.join().unwrap();
    let after = gui.settle(&Settle::default(), Some(WAIT)).unwrap();
    assert!(after.settled);
}

#[test]
fn info_shows_the_sessions_only_to_a_peer_with_a_gui_permission() {
    let seen = world(
        gui_config(),
        Grant::allowing([Permission::Info, Permission::GuiView]),
    );
    let info = Control::open(&seen.link).unwrap().info(Some(WAIT)).unwrap();
    assert!(info.gui.enabled && info.gui.available);
    assert_eq!(info.gui.backend.as_deref(), Some("painted"));
    assert_eq!(info.gui.sessions.len(), 1);
    assert_eq!(info.gui.sessions[0].displays.len(), 2);
    assert_eq!(info.gui.sessions[0].user.as_deref(), Some("painter"));
    let blind = world(gui_config(), Grant::allowing([Permission::Info]));
    let info = Control::open(&blind.link)
        .unwrap()
        .info(Some(WAIT))
        .unwrap();
    // A peer that may only ask `info` is told that the host serves a desktop and in what formats, and nothing the desktop says.
    assert!(info.gui.enabled && !info.gui.available);
    assert!(info.gui.backend.is_none() && info.gui.reason.is_none() && !info.gui.input);
    assert!(info.gui.sessions.is_empty());
    let off = world(
        GuiConfig::default(),
        Grant::allowing([Permission::Info, Permission::GuiView]),
    );
    let info = Control::open(&off.link).unwrap().info(Some(WAIT)).unwrap();
    assert!(!info.gui.enabled && !info.gui.available && info.gui.sessions.is_empty());
    let refused = Gui::open(&off.link, &GuiRequest::default()).unwrap_err();
    assert_eq!(remote(&refused).code, ErrorCode::Unsupported);
}

#[test]
fn the_owners_events_hold_counts_and_never_what_was_typed() {
    let world = world(gui_config(), all());
    let secret = "correct-horse-battery-staple";
    {
        let gui = open(&world, &[Want::View, Want::Input, Want::Clipboard]);
        gui.input(
            &[InputEvent::Text {
                text: secret.into(),
            }],
            Some(WAIT),
        )
        .unwrap();
        gui.set_clipboard(secret, Some(WAIT)).unwrap();
        gui.clipboard(Some(WAIT)).unwrap();
        gui.snapshot(&SnapshotOptions::default(), Some(WAIT))
            .unwrap();
        gui.close();
    }
    let until = Instant::now() + WAIT;
    let ended = loop {
        let events = world.host.events.0.lock().unwrap().clone();
        if let Some(found) = events.iter().find_map(|event| match event {
            NodeEvent::Gui {
                change: GuiChange::Ended { .. },
                ..
            } => Some(event.clone()),
            _ => None,
        }) {
            break (found, events);
        }
        assert!(Instant::now() < until, "the stream's end was never told");
        thread::sleep(Duration::from_millis(20));
    };
    let (found, events) = ended;
    let NodeEvent::Gui {
        change:
            GuiChange::Ended {
                snapshots,
                input_events,
                clipboard_reads,
                clipboard_writes,
                ..
            },
        ..
    } = found
    else {
        unreachable!()
    };
    assert_eq!(
        (snapshots, input_events, clipboard_reads, clipboard_writes),
        (1, 1, 1, 1)
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            NodeEvent::Gui {
                change: GuiChange::ClipboardRead,
                ..
            }
        )),
        "a clipboard read is its own event"
    );
    let text = format!("{events:?}");
    assert!(!text.contains(secret), "an event carried what was typed");
}

#[test]
fn input_is_refused_while_the_lock_state_is_unknown_unless_the_host_allows_it() {
    let text = || InputEvent::Text { text: "pw".into() };
    let unknown = world(gui_config(), all());
    unknown.desktop.set_locked(None);
    let gui = open(&unknown, &[Want::View, Want::Input]);
    let refused = gui.input(&[text()], Some(WAIT)).unwrap_err();
    assert_eq!(remote(&refused).code, ErrorCode::Unavailable);
    assert!(
        remote(&refused).message.contains("not known"),
        "{refused:?}"
    );
    assert!(unknown.desktop.log().is_empty());

    // The same desktop, once the host says to type anyway, and a desktop that says it is not locked, with the default.
    let allowed = world(
        GuiConfig {
            input_when_lock_unknown: true,
            ..gui_config()
        },
        all(),
    );
    allowed.desktop.set_locked(None);
    let gui = open(&allowed, &[Want::Input]);
    assert_eq!(gui.input(&[text()], Some(WAIT)).unwrap(), 1);
    let known = world(gui_config(), all());
    known.desktop.set_locked(Some(false));
    let gui = open(&known, &[Want::Input]);
    assert_eq!(gui.input(&[text()], Some(WAIT)).unwrap(), 1);
}

#[test]
fn input_is_costed_by_what_it_holds_the_desktop_for_and_a_settle_wait_is_bounded() {
    let config = GuiConfig {
        max_input_rate: 100,
        max_settle_wait: bohio::config::Span(Duration::from_secs(2)),
        ..gui_config()
    };
    let world = world(config, all());
    let gui = open(&world, &[Want::View, Want::Input]);
    // One text event of more characters than a second's input is busy, and so is a wheel of many notches.
    let long = InputEvent::Text {
        text: "a".repeat(101),
    };
    assert_eq!(
        remote(&gui.input(&[long], Some(WAIT)).unwrap_err()).code,
        ErrorCode::Busy
    );
    let wheel = InputEvent::Scroll {
        display: None,
        x: None,
        y: None,
        dx: -60,
        dy: 60,
    };
    assert_eq!(
        remote(&gui.input(&[wheel], Some(WAIT)).unwrap_err()).code,
        ErrorCode::Busy
    );
    assert!(world.desktop.log().is_empty());
    // A settle that waits ten times what the host allows is refused naming the key; the allowed wait is served.
    let greedy = Settle {
        timeout_ms: Some(20_000),
        ..Settle::default()
    };
    let refused = gui.settle(&greedy, Some(WAIT)).unwrap_err();
    assert_eq!(remote(&refused).code, ErrorCode::BadRequest);
    assert!(
        remote(&refused).message.contains("timeout_ms"),
        "{refused:?}"
    );
    assert!(
        remote(&refused).message.contains("max_settle_wait"),
        "{refused:?}"
    );
    let fine = Settle {
        timeout_ms: Some(1_000),
        ..Settle::default()
    };
    assert!(gui.settle(&fine, Some(WAIT)).is_ok());
}
