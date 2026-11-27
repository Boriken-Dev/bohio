//! A watch of a host's desktop over `bohio-gui/1`: tiles, codecs, flow control and the adaptive ladder, against the painted
//! in-memory desktop. The first tests run over memory pipes; the last runs a host over loopback TCP behind a relay that
//! limits its rate and delays it, as a slow link does.

#![forbid(unsafe_code)]

#[path = "../examples/support/painted.rs"]
mod painted;

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{GuiConfig, LinkConfig, NodeConfig, Span};
use bohio::listen::Listening;
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_client::Error;
use bohio_client::connect::Connection;
use bohio_client::gui::{Gui, Watch, WatchEvent};
use bohio_client::link::{Settings, stream};
use bohio_proto::ErrorCode;
use bohio_proto::gui::{
    GuiRequest, InputEvent, Rect, RectEncoding, UpdateHeader, Want, WatchEncoding,
};
use cabuya::auth::{Identity, Method, Proof, Trust};
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure, StreamChannel, memory};
use painted::{Painted, pattern};

const WAIT: Duration = Duration::from_secs(30);
const RECHECK: Duration = Duration::from_millis(400);

struct Peer {
    node: Arc<Node>,
    id: PeerId,
    keys: Arc<MemoryKeys>,
    grants: Arc<MemoryGrants>,
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

struct World {
    desktop: Arc<Painted>,
    host: Arc<Peer>,
    client: Arc<Peer>,
    link: PeerLink,
    _wire: Wire,
}

fn config() -> GuiConfig {
    GuiConfig {
        enabled: true,
        // A watch polls the connection often enough that a test does not wait on the default.
        poll_interval: Span(Duration::from_millis(10)),
        ..GuiConfig::default()
    }
}

fn world(gui: GuiConfig, grant: Grant) -> World {
    let desktop = Arc::new(Painted::new());
    let host = Peer::new(gui, Some(desktop.clone()));
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

fn viewer() -> Grant {
    Grant::allowing([Permission::Info, Permission::GuiView, Permission::GuiInput])
}

fn open(world: &World, want: &[Want]) -> Gui {
    Gui::open(&world.link, &GuiRequest::new(want.iter().copied())).expect("the stream opens")
}

/// Stops a thread that paints until told to, even when the test panics.
struct Painter {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Painter {
    /// Fills a 64 by 64 corner with a new colour every few milliseconds.
    fn start(desktop: &Arc<Painted>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let (desktop, stop) = (desktop.clone(), stop.clone());
            move || {
                let mut count = 0_u32;
                while !stop.load(Ordering::SeqCst) {
                    count += 1;
                    desktop.paint_with(0, Rect::new(0, 0, 64, 64), |x, y| {
                        [count as u8, x as u8, (y + count) as u8]
                    });
                    thread::sleep(Duration::from_millis(5));
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn lossless(fps: u32) -> Watch {
    Watch {
        fps: Some(fps),
        encoding: Some(WatchEncoding::Lossless),
        ..Watch::default()
    }
}

/// The next update, skipping pointer frames; `None` when none comes within `within`.
fn update(gui: &Gui, within: Duration) -> Option<(UpdateHeader, u64)> {
    let until = Instant::now() + within;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        match gui.next_event(Some(left)) {
            Ok(WatchEvent::Update(info)) => return Some((info.header, info.bytes)),
            Ok(WatchEvent::Cursor(_)) => {}
            Err(Error::Timeout) => return None,
            Err(error) => panic!("the watch failed: {error:?}"),
        }
    }
}

/// Reads updates until the composed frame equals the desktop's display `display`, or panics after `WAIT`.
fn until_equal(gui: &Gui, desktop: &Painted, display: usize) -> Vec<(UpdateHeader, u64)> {
    let until = Instant::now() + WAIT;
    let mut seen = Vec::new();
    loop {
        if !seen.is_empty()
            && gui
                .screen()
                .is_some_and(|screen| screen.rgb == desktop.pixels(display).2)
        {
            return seen;
        }
        assert!(
            Instant::now() < until,
            "the frame never equalled the desktop"
        );
        if let Some(one) = update(gui, Duration::from_millis(500)) {
            seen.push(one);
        }
    }
}

#[test]
fn a_lossless_watch_of_a_moving_square_composes_to_the_desktops_frame() {
    let world = world(config(), viewer());
    let gui = open(&world, &[Want::View]);
    gui.watch(&lossless(15)).unwrap();
    let first = until_equal(&gui, &world.desktop, 0);
    let (header, _) = &first[0];
    assert!(header.key, "the first update is a key frame");
    assert_eq!((header.width, header.height), (320, 200));
    assert!(header.scale.to_bits() == 1.0_f64.to_bits());
    assert!(
        first[0]
            .0
            .rects
            .iter()
            .all(|part| matches!(part.encoding, RectEncoding::ZstdRgb | RectEncoding::RawRgb)),
        "lossless is never JPEG"
    );
    let full = 320 * 200 * 3;
    let mut moved_bytes = Vec::new();
    for step in 0..8_u32 {
        // Put the background back where the square was, then paint it a step on.
        if step > 0 {
            let old = 10 + (step - 1) * 20;
            world
                .desktop
                .paint_with(0, Rect::new(old, 80, 30, 30), |x, y| pattern(0, x, y));
        }
        world.desktop.paint(
            0,
            Rect::new(10 + step * 20, 80, 30, 30),
            [250, (step * 30) as u8, 9],
        );
        let seen = until_equal(&gui, &world.desktop, 0);
        for (header, bytes) in &seen {
            assert!(!header.key);
            let area: u64 = header
                .rects
                .iter()
                .map(|part| u64::from(part.width) * u64::from(part.height))
                .sum();
            // Only tiles near the square: far less than the frame.
            assert!(
                area <= 64 * 64 * 6,
                "{area} pixels in an update for a 30 pixel square"
            );
            moved_bytes.push(*bytes);
        }
    }
    let largest = moved_bytes.iter().max().copied().unwrap();
    assert!(
        largest < full / 4,
        "an update of a moving square took {largest} bytes of a frame of {full}"
    );
    // The last update's frame is the desktop's, exactly.
    assert_eq!(gui.screen().unwrap().rgb, world.desktop.pixels(0).2);
}

#[test]
fn never_more_than_frames_in_flight_are_unacknowledged_and_a_silent_client_gets_no_more() {
    let world = world(config(), viewer());
    let _painter = Painter::start(&world.desktop);
    let gui = open(&world, &[Want::View]);
    gui.set_auto_ack(false);
    gui.watch(&lossless(15)).unwrap();
    // Without an acknowledgement exactly the two updates in flight arrive, and then nothing, however long it waits.
    let mut unacknowledged = Vec::new();
    while let Some((header, _)) = update(&gui, Duration::from_millis(1200)) {
        unacknowledged.push(header.frame);
        assert!(unacknowledged.len() <= 2, "{unacknowledged:?}");
    }
    assert_eq!(unacknowledged, [1, 2]);
    // Each acknowledgement makes room for one more, never two.
    let mut most = 2;
    for next in 3..9 {
        let oldest = unacknowledged.remove(0);
        gui.ack(oldest).unwrap();
        let (header, _) = update(&gui, Duration::from_secs(10)).expect("room made an update");
        assert_eq!(header.frame, next);
        unacknowledged.push(header.frame);
        most = most.max(unacknowledged.len());
        assert!(update(&gui, Duration::from_millis(300)).is_none());
    }
    assert_eq!(most, 2);
    // The setting is the bound: one in flight means one.
    drop(gui);
    let single = self::world(
        GuiConfig {
            frames_in_flight: 1,
            ..config()
        },
        viewer(),
    );
    let _painter = Painter::start(&single.desktop);
    let gui = open(&single, &[Want::View]);
    gui.set_auto_ack(false);
    gui.watch(&lossless(15)).unwrap();
    assert!(update(&gui, Duration::from_secs(10)).is_some());
    assert!(update(&gui, Duration::from_millis(800)).is_none());
}

#[test]
fn removing_gui_view_ends_a_watch_within_the_recheck() {
    let world = world(config(), viewer());
    let _painter = Painter::start(&world.desktop);
    let gui = open(&world, &[Want::View]);
    gui.watch(&lossless(15)).unwrap();
    assert!(update(&gui, Duration::from_secs(10)).is_some());
    world
        .host
        .grants
        .set(world.client.id, Grant::allowing([Permission::Info]));
    let started = Instant::now();
    let ended = loop {
        match gui.next_event(Some(Duration::from_millis(200))) {
            Ok(_) | Err(Error::Timeout) => {}
            Err(error) => break error,
        }
        assert!(started.elapsed() < WAIT, "the watch was never ended");
    };
    assert!(
        matches!(ended, Error::Transport(Failure::Permission)),
        "{ended:?}"
    );
    assert!(started.elapsed() < RECHECK * 2 + Duration::from_secs(3));
}

#[test]
fn an_unchanged_screen_sends_nothing_and_a_key_request_sends_the_whole_frame() {
    let world = world(config(), viewer());
    let gui = open(&world, &[Want::View]);
    gui.watch(&lossless(15)).unwrap();
    let (first, _) = update(&gui, Duration::from_secs(10)).expect("a key frame");
    assert!(first.key);
    assert_eq!(first.rects.len(), 1);
    assert!(
        update(&gui, Duration::from_millis(700)).is_none(),
        "nothing changed"
    );
    let mut again = lossless(15);
    again.key = true;
    gui.watch(&again).unwrap();
    let (second, _) = update(&gui, Duration::from_secs(10)).expect("another key frame");
    assert!(second.key);
    assert_eq!(second.frame, first.frame + 1);
    let part = second.rects[0];
    assert_eq!((part.x, part.y, part.width, part.height), (0, 0, 320, 200));
    gui.unwatch().unwrap();
    // What was sent before the host heard may still arrive.
    thread::sleep(Duration::from_millis(300));
    while update(&gui, Duration::from_millis(300)).is_some() {}
    world.desktop.paint(0, Rect::new(5, 5, 10, 10), [1, 2, 3]);
    assert!(
        update(&gui, Duration::from_millis(700)).is_none(),
        "no update after unwatch"
    );
}

#[test]
fn a_jpeg_rectangle_is_resent_lossless_once_it_stays_unchanged() {
    let gui_config = GuiConfig {
        // Nothing compresses this well, so the host chooses JPEG; and a short wait before the exact copy.
        lossless_max_bpp: 0.000_1,
        refine_after: Span(Duration::from_millis(300)),
        ..config()
    };
    let world = world(gui_config, viewer());
    let gui = open(&world, &[Want::View]);
    gui.watch(&Watch {
        fps: Some(15),
        ..Watch::default()
    })
    .unwrap();
    let (first, _) = update(&gui, Duration::from_secs(10)).expect("a key frame");
    assert!(first.key);
    assert!(
        first
            .rects
            .iter()
            .all(|part| part.encoding == RectEncoding::Jpeg)
    );
    assert_ne!(
        gui.screen().unwrap().rgb,
        world.desktop.pixels(0).2,
        "a JPEG is not exact"
    );
    let seen = until_equal(&gui, &world.desktop, 0);
    let (refined, _) = &seen[0];
    assert!(!refined.key);
    assert!(
        refined
            .rects
            .iter()
            .all(|part| matches!(part.encoding, RectEncoding::ZstdRgb | RectEncoding::RawRgb))
    );
}

#[test]
fn a_watch_fits_the_frame_to_the_size_asked_and_reports_the_scale() {
    let world = world(config(), viewer());
    let gui = open(&world, &[Want::View]);
    gui.watch(&Watch {
        max_width: Some(160),
        encoding: Some(WatchEncoding::Lossless),
        ..Watch::default()
    })
    .unwrap();
    let (header, _) = update(&gui, Duration::from_secs(10)).expect("a key frame");
    assert_eq!((header.width, header.height), (160, 100));
    assert!((header.scale - 0.5).abs() < 1e-9);
    let screen = gui.screen().unwrap();
    assert_eq!((screen.width, screen.height), (160, 100));
}

#[test]
fn the_pointer_alone_is_a_cursor_frame() {
    let world = world(config(), viewer());
    let gui = open(&world, &[Want::View, Want::Input]);
    gui.watch(&Watch {
        cursor: true,
        encoding: Some(WatchEncoding::Lossless),
        ..Watch::default()
    })
    .unwrap();
    let (first, _) = update(&gui, Duration::from_secs(10)).expect("a key frame");
    assert!(first.cursor.is_some());
    gui.input(
        &[InputEvent::Move {
            display: None,
            x: 100,
            y: 50,
        }],
        Some(WAIT),
    )
    .unwrap();
    let until = Instant::now() + WAIT;
    loop {
        match gui.next_event(Some(Duration::from_millis(500))) {
            Ok(WatchEvent::Cursor(cursor)) => {
                assert_eq!((cursor.x, cursor.y, cursor.visible), (100, 50, true));
                break;
            }
            Ok(WatchEvent::Update(info)) => {
                assert_eq!(
                    info.header.cursor.map(|point| (point.x, point.y)),
                    Some((100, 50))
                );
                break;
            }
            Err(Error::Timeout) => assert!(Instant::now() < until, "no cursor frame"),
            Err(error) => panic!("{error:?}"),
        }
    }
}

#[test]
fn a_watch_needs_gui_view_and_a_setting_that_means_nothing_is_refused() {
    let world = world(
        config(),
        Grant::allowing([Permission::Info, Permission::GuiInput]),
    );
    let gui = open(&world, &[Want::Input]);
    gui.watch(&lossless(5)).unwrap();
    let denied = gui.next_event(Some(WAIT)).unwrap_err();
    match denied {
        Error::Remote(body) => assert_eq!(body.code, ErrorCode::Denied),
        other => panic!("{other:?}"),
    }

    let world = self::world(config(), viewer());
    let gui = open(&world, &[Want::View]);
    for options in [
        Watch {
            fps: Some(0),
            ..Watch::default()
        },
        Watch {
            quality: Some(0),
            ..Watch::default()
        },
        Watch {
            max_rate: Some(0),
            ..Watch::default()
        },
        Watch {
            max_width: Some(0),
            ..Watch::default()
        },
    ] {
        gui.watch(&options).unwrap();
        match gui.next_event(Some(WAIT)) {
            Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::BadRequest, "{options:?}"),
            other => panic!("{options:?}: {other:?}"),
        }
    }
    // A request for more than the host allows gets what it allows.
    gui.watch(&lossless(1000)).unwrap();
    assert!(update(&gui, Duration::from_secs(10)).is_some());
}

/// How many updates and bytes a watch of the painter's corner gets in `seconds`, and what its first update took.
fn corner_watch(rate: Option<u64>, seconds: u64) -> (u64, u64, u64) {
    let world = world(config(), viewer());
    let _painter = Painter::start(&world.desktop);
    let gui = open(&world, &[Want::View]);
    gui.watch(&Watch {
        fps: Some(15),
        region: Some(Rect::new(0, 0, 64, 64)),
        encoding: Some(WatchEncoding::Lossless),
        max_rate: rate,
        ..Watch::default()
    })
    .unwrap();
    let started = Instant::now();
    let (mut count, mut bytes, mut first) = (0, 0, 0);
    while started.elapsed() < Duration::from_secs(seconds) {
        if let Some((_, size)) = update(&gui, Duration::from_millis(300)) {
            if count == 0 {
                first = size;
            }
            count += 1;
            bytes += size;
        }
    }
    (count, bytes, first)
}

#[test]
fn the_byte_rate_cap_spaces_the_updates() {
    let cap = 8_000;
    let seconds = 6;
    let (free_count, free_bytes, _) = corner_watch(None, seconds);
    let (count, bytes, first) = corner_watch(Some(cap), seconds);
    eprintln!(
        "uncapped {free_count} updates {free_bytes} bytes; capped at {cap} B/s {count} updates {bytes} bytes"
    );
    assert!(count >= 2, "the cap stopped the watch");
    // The first update goes out unpaced; every later one waits for what the ones before cost.
    assert!(
        bytes - first <= cap * seconds * 5 / 4,
        "{} bytes in {seconds} s at a cap of {cap} B/s",
        bytes - first
    );
    assert!(
        free_bytes > bytes * 3,
        "uncapped {free_bytes} against capped {bytes}"
    );
}

// ---- a slow link ------------------------------------------------------------------------------------------------

/// A relay between a client and a host that passes at most `rate` bytes a second each way and delays each chunk by
/// `latency`, as a slow link does.
struct Throttle {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<TcpStream>>>,
    threads: Vec<JoinHandle<()>>,
}

impl Throttle {
    fn to(target: SocketAddr, rate: u64, latency: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        listener.set_nonblocking(true).expect("non-blocking");
        let address = listener.local_addr().expect("an address");
        let stop = Arc::new(AtomicBool::new(false));
        let connections: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let accept = thread::spawn({
            let (stop, connections) = (stop.clone(), connections.clone());
            move || {
                let mut pumps = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    let Ok((from, _)) = listener.accept() else {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    let _ = from.set_nonblocking(false);
                    let Ok(to) = TcpStream::connect(target) else {
                        continue;
                    };
                    let _ = (from.set_nodelay(true), to.set_nodelay(true));
                    for (source, sink) in [
                        (from.try_clone().unwrap(), to.try_clone().unwrap()),
                        (to.try_clone().unwrap(), from.try_clone().unwrap()),
                    ] {
                        pumps.extend(pump(source, sink, rate, latency));
                    }
                    let mut held = connections.lock().unwrap();
                    held.push(from);
                    held.push(to);
                }
                for handle in pumps {
                    let _ = handle.join();
                }
            }
        });
        Self {
            address,
            stop,
            connections,
            threads: vec![accept],
        }
    }
}

/// Moves bytes from `source` to `sink`: a reader stamps each chunk with its arrival and a writer sends it `latency` later,
/// no faster than `rate` bytes a second.
fn pump(
    mut source: TcpStream,
    mut sink: TcpStream,
    rate: u64,
    latency: Duration,
) -> Vec<JoinHandle<()>> {
    let (send, receive) = mpsc::channel::<(Instant, Vec<u8>)>();
    let reader = thread::spawn(move || {
        let mut buffer = [0_u8; 1024];
        while let Ok(count) = source.read(&mut buffer) {
            if count == 0
                || send
                    .send((Instant::now(), buffer[..count].to_vec()))
                    .is_err()
            {
                break;
            }
        }
    });
    let writer = thread::spawn(move || {
        let mut ready = Instant::now();
        while let Ok((arrived, chunk)) = receive.recv() {
            let now = Instant::now();
            let at = (arrived + latency).max(ready).max(now);
            thread::sleep(at - now);
            if sink.write_all(&chunk).is_err() {
                break;
            }
            ready = at + Duration::from_secs_f64(chunk.len() as f64 / rate as f64);
        }
        let _ = sink.shutdown(Shutdown::Both);
    });
    vec![reader, writer]
}

impl Drop for Throttle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for connection in self.connections.lock().unwrap().drain(..) {
            let _ = connection.shutdown(Shutdown::Both);
        }
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

/// A host over loopback TCP with a painted desktop, and one granted peer.
struct TcpHost {
    desktop: Arc<Painted>,
    id: PeerId,
    address: SocketAddr,
    client: Identity,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TcpHost {
    fn start(gui: GuiConfig) -> Self {
        let config = NodeConfig {
            link: LinkConfig {
                recheck: Span(Duration::from_millis(500)),
                ..LinkConfig::default()
            },
            gui,
            ..NodeConfig::default()
        };
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let desktop = Arc::new(Painted::new());
        let node = Arc::new(
            Node::new(config.clone(), identity, keys.clone(), grants.clone())
                .expect("the node is built")
                .with_desktop(desktop.clone()),
        );
        let client = Identity::generate(Method::DEFAULT).expect("an identity");
        keys.accept(client.peer_id());
        grants.set(client.peer_id(), viewer());
        let entry = bohio::config::ListenConfig {
            endpoint: "tcp://127.0.0.1:0".to_owned(),
            ..bohio::config::ListenConfig::default()
        };
        let (listening, bound) =
            Listening::bind(&entry, &config.link.settings()).expect("the listener binds");
        let address = bound
            .to_string()
            .strip_prefix("tcp://")
            .expect("a tcp endpoint")
            .parse()
            .expect("an address");
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let _ = node.serve_listening(&listening, &stop);
            }
        });
        Self {
            desktop,
            id,
            address,
            client,
            stop,
            thread: Some(thread),
        }
    }

    fn connect(&self, through: SocketAddr) -> Connection {
        let proof = Proof::builder(self.client.clone())
            .responders(Trust::pinned([self.id]))
            .build()
            .expect("a proof");
        Connection::connect_tcp(
            through,
            Duration::from_secs(10),
            &proof,
            &Settings::default(),
        )
        .expect("the link is up")
    }
}

impl Drop for TcpHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What a watch of `seconds` measured.
#[derive(Debug, Default)]
struct Measured {
    updates: u64,
    bytes: u64,
    first_bytes: Vec<u64>,
    last_bytes: Vec<u64>,
    max_level: u32,
    last_level: u32,
    ack_ms: Vec<u64>,
    seconds: f64,
}

impl Measured {
    fn mean(bytes: &[u64]) -> f64 {
        bytes.iter().sum::<u64>() as f64 / bytes.len().max(1) as f64
    }

    fn describe(&self, label: &str) -> String {
        format!(
            "{label}: {:.1} updates/s, {:.0} bytes/update ({:.0} B/s), host ack delay {} ms, level {} (max {})",
            self.updates as f64 / self.seconds,
            self.bytes as f64 / self.updates.max(1) as f64,
            self.bytes as f64 / self.seconds,
            self.ack_ms.last().copied().unwrap_or(0),
            self.last_level,
            self.max_level,
        )
    }
}

/// Drives a watch of the noisy moving block for `seconds` through `address`.
fn measure(host: &TcpHost, address: SocketAddr, options: &Watch, seconds: u64) -> Measured {
    let connection = host.connect(address);
    let gui = Gui::open(&connection, &GuiRequest::new([Want::View])).expect("the stream opens");
    let painting = Arc::new(AtomicBool::new(true));
    let painter = thread::spawn({
        let (desktop, painting) = (host.desktop.clone(), painting.clone());
        move || {
            let mut step = 0_u32;
            while painting.load(Ordering::SeqCst) {
                step += 1;
                // A block of noise a photograph's worth of detail, moving.
                let left = (step * 3) % 150;
                desktop.paint_with(0, Rect::new(left, 20, 160, 120), |x, y| {
                    let hash =
                        (x + 1).wrapping_mul(2_654_435_761) ^ (y + step).wrapping_mul(40_503);
                    [hash as u8, (hash >> 8) as u8, (hash >> 16) as u8]
                });
                thread::sleep(Duration::from_millis(20));
            }
        }
    });
    gui.watch(options).unwrap();
    let mut measured = Measured::default();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) {
        match gui.next_event(Some(Duration::from_millis(500))) {
            Ok(WatchEvent::Update(info)) => {
                measured.updates += 1;
                measured.bytes += info.bytes;
                if measured.first_bytes.len() < 3 {
                    measured.first_bytes.push(info.bytes);
                }
                measured.last_bytes.push(info.bytes);
                if measured.last_bytes.len() > 4 {
                    measured.last_bytes.remove(0);
                }
                measured.max_level = measured.max_level.max(info.header.level);
                measured.last_level = info.header.level;
                if let Some(ms) = info.header.ack_ms {
                    measured.ack_ms.push(ms);
                }
            }
            Ok(WatchEvent::Cursor(_)) | Err(Error::Timeout) => {}
            Err(error) => panic!("the watch failed: {error:?}"),
        }
    }
    measured.seconds = started.elapsed().as_secs_f64();
    painting.store(false, Ordering::SeqCst);
    painter.join().unwrap();
    gui.close();
    measured
}

#[test]
fn a_slow_link_steps_the_ladder_down_and_a_fast_one_does_not() {
    // Local sanity check, not a measurement: the link is a relay on this machine.
    let host = TcpHost::start(GuiConfig {
        // A late acknowledgement is judged against the requested rate: 4 updates a second, 250 ms apart.
        ..config()
    });
    let options = Watch {
        fps: Some(4),
        encoding: Some(WatchEncoding::Jpeg),
        ..Watch::default()
    };
    // Control: the host reached directly.
    let fast = measure(&host, host.address, &options, 6);
    eprintln!("{}", fast.describe("direct"));
    // A link of 60 kB/s with 40 ms each way.
    let slow_link = Throttle::to(host.address, 60_000, Duration::from_millis(40));
    let slow = measure(&host, slow_link.address, &options, 14);
    eprintln!("{}", slow.describe("60 kB/s + 80 ms rtt"));
    eprintln!(
        "bytes per update, first three {:?}, last four {:?}",
        slow.first_bytes, slow.last_bytes
    );
    assert_eq!(fast.max_level, 0, "an unhindered link keeps full quality");
    assert!(fast.updates >= 10, "{fast:?}");
    assert!(
        slow.max_level >= 1,
        "the slow link never stepped down: {slow:?}"
    );
    assert!(
        Measured::mean(&slow.last_bytes) < Measured::mean(&slow.first_bytes),
        "updates did not get cheaper: {slow:?}"
    );
    // The host's own average of how late acknowledgements were stays bounded: two updates in flight, no queue.
    let worst = slow.ack_ms.iter().max().copied().unwrap_or(0);
    assert!(worst < 10_000, "{slow:?}");
}

#[test]
fn a_display_that_changes_size_between_state_polls_reshapes_the_watch() {
    // The state is polled far less often than the watch captures, so the capture finds the new size first.
    let gui_config = GuiConfig {
        state_interval: Span(Duration::from_secs(600)),
        ..config()
    };
    let world = world(gui_config, viewer());
    let gui = open(&world, &[Want::View]);
    gui.watch(&lossless(15)).unwrap();
    let (first, _) = update(&gui, Duration::from_secs(10)).expect("a key frame");
    assert!(first.key);
    world.desktop.resize(0, 200, 120);
    let (second, _) = update(&gui, Duration::from_secs(10)).expect("a key frame of the new size");
    assert!(second.key);
    let part = second.rects[0];
    assert_eq!((part.x, part.y, part.width, part.height), (0, 0, 200, 120));
    gui.unwatch().unwrap();
}
