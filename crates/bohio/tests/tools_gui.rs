//! The desktop tools of `bohio_client::tools` against a host with a painted in-memory desktop: no screen is touched.

#![forbid(unsafe_code)]

#[path = "../examples/support/painted.rs"]
mod painted;

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{GuiConfig, LinkConfig, NodeConfig, Span};
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_client::link::{Settings, stream};
use bohio_client::tools::{
    Availability, Cancel, Output, Settings as ToolSettings, ToolError, Tools,
};
use coqui_link::auth::{Identity, Method};
use coqui_peer::PeerId;
use coqui_transport::{Channel, StreamChannel, memory};
use painted::Painted;
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);

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

struct Wire {
    channels: [Arc<StreamChannel>; 2],
    threads: Vec<JoinHandle<()>>,
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
    tools: Tools,
    _wire: Wire,
}

fn world(config: GuiConfig, grant: Grant) -> World {
    world_with(config, grant, ToolSettings::default())
}

fn world_with(config: GuiConfig, grant: Grant, tool_settings: ToolSettings) -> World {
    let desktop = Arc::new(Painted::new());
    let host = Peer::new(config, Some(desktop.clone()));
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
    let wire = Wire {
        channels: [dial_end, answer_end],
        threads,
    };
    let until = Instant::now() + WAIT;
    let link: PeerLink = loop {
        if let Some(link) = client.node.peer(&host.id) {
            break link;
        }
        assert!(Instant::now() < until, "no link came up");
        thread::sleep(Duration::from_millis(20));
    };
    let tools = Tools::with_settings(Arc::new(link), tool_settings).expect("the tools are made");
    World {
        desktop,
        tools,
        _wire: wire,
    }
}

fn on() -> GuiConfig {
    GuiConfig {
        enabled: true,
        ..GuiConfig::default()
    }
}

fn all() -> Grant {
    Grant::allowing([Permission::Info, Permission::GuiView, Permission::GuiInput])
}

fn call(world: &World, name: &str, arguments: Value) -> Result<Output, ToolError> {
    world.tools.call(name, arguments, &Cancel::new())
}

fn after_action() -> ToolSettings {
    let mut settings = ToolSettings::default();
    settings.gui_after_action = true;
    settings
}

fn log(world: &World) -> Vec<String> {
    world.desktop.log()
}

#[test]
fn a_screenshot_fits_and_a_click_lands_at_the_scaled_point() {
    let world = world(on(), all());
    let shot = call(
        &world,
        "screenshot",
        json!({"format": "png", "max_width": 160, "max_height": 100}),
    )
    .expect("a screenshot");
    assert_eq!(shot.images.len(), 1);
    assert_eq!(shot.images[0].mime, "image/png");
    assert_eq!(&shot.images[0].data[..4], b"\x89PNG");
    assert_eq!(shot.value["width"], 160, "{}", shot.value);
    assert_eq!(shot.value["height"], 100);
    assert_eq!(shot.value["scale"], 0.5);
    assert_eq!(shot.value["display"], "0");
    assert_eq!(shot.value["settled"], true, "a screenshot settles first");

    // (80, 50) of the half-size picture is (160, 100) of the display.
    let click = call(&world, "click", json!({"x": 80, "y": 50})).expect("a click");
    assert_eq!(click.value["applied"], 1);
    assert_eq!(log(&world), ["click:leftx1@0:160,100"]);
    // The painted mark is where the display's pixel is, not where the picture's was.
    let (width, _, pixels) = world.desktop.pixels(0);
    let at = ((100 * width + 160) * 3) as usize;
    assert_eq!(&pixels[at..at + 3], &painted::CLICK);

    // Raw display pixels need no screenshot.
    call(
        &world,
        "click",
        json!({"x": 10, "y": 20, "space": "display", "button": "right", "count": 2}),
    )
    .expect("a click in display pixels");
    assert_eq!(log(&world)[1], "click:rightx2@0:10,20");
}

#[test]
fn a_region_screenshot_maps_back_through_its_origin_and_scale() {
    let world = world(on(), all());
    let shot = call(
        &world,
        "screenshot",
        json!({"region": {"x": 100, "y": 40, "width": 80, "height": 40}, "max_width": 40, "max_height": 40}),
    )
    .expect("a region");
    assert_eq!(shot.value["width"], 40);
    assert_eq!(shot.value["scale"], 0.5);
    assert_eq!(shot.value["region"]["x"], 100);
    // Picture (10, 10) is region (20, 20), display (120, 60).
    call(&world, "move_pointer", json!({"x": 10, "y": 10})).expect("a move");
    assert_eq!(log(&world), ["move:0@120,60"]);
}

#[test]
fn a_point_outside_the_display_names_the_bounds_and_nothing_reaches_the_desktop() {
    let world = world(on(), all());
    let none = call(&world, "click", json!({"x": 1, "y": 1})).expect_err("no screenshot yet");
    assert_eq!(none.code, "no_screenshot", "{none}");
    call(
        &world,
        "screenshot",
        json!({"max_width": 160, "max_height": 100}),
    )
    .expect("a screenshot");
    let shown = call(&world, "click", json!({"x": 500, "y": 10})).expect_err("outside the picture");
    assert_eq!(shown.code, "out_of_bounds", "{shown}");
    assert!(shown.message.contains("160 by 100"), "{shown}");
    assert!(shown.message.contains("320 by 200"), "{shown}");
    let display = call(
        &world,
        "click",
        json!({"x": 320, "y": 10, "space": "display"}),
    )
    .expect_err("outside the display");
    assert_eq!(display.code, "out_of_bounds", "{display}");
    assert!(display.message.contains("320 by 200"), "{display}");
    assert!(display.message.contains("x 0 to 319"), "{display}");
    let other = call(&world, "click", json!({"x": 1, "y": 1, "display": "9"}))
        .expect_err("no such display");
    assert_eq!(other.code, "bad_request");
    assert!(other.message.contains("0, 1"), "{other}");
    assert!(log(&world).is_empty());
}

#[test]
fn keys_text_scroll_and_drag_reach_the_desktop_in_order() {
    let world = world(on(), all());
    call(&world, "key", json!({"keys": "ctrl+s"})).expect("a combination");
    call(&world, "type", json!({"text": "hello"})).expect("text");
    call(
        &world,
        "scroll",
        json!({"x": 30, "y": 40, "dy": 3, "space": "display"}),
    )
    .expect("a scroll");
    call(
        &world,
        "drag",
        json!({"from": {"x": 10, "y": 20}, "to": {"x": 60, "y": 70}, "space": "display"}),
    )
    .expect("a drag");
    assert_eq!(
        log(&world),
        [
            "key:control:press",
            "key:s:click",
            "key:control:release",
            "text:hello",
            "scroll:0,3",
            "press:leftx1@0:10,20",
            "move:0@60,70",
            "release:leftx1@0:60,70",
        ]
    );
    assert_eq!(
        call(&world, "key", json!({"keys": "ctrl+nonsense"}))
            .unwrap_err()
            .code,
        "bad_request"
    );
    assert!(call(&world, "scroll", json!({"x": 1, "dy": 1})).is_err());
    assert!(call(&world, "scroll", json!({})).is_err());
    assert!(call(&world, "type", json!({"text": ""})).is_err());
}

#[test]
fn wait_settles_displays_lists_the_layout_and_the_cursor_follows_the_pointer() {
    let world = world(on(), all());
    let waited =
        call(&world, "wait", json!({"quiet_ms": 100, "timeout_ms": 3000})).expect("a wait");
    assert_eq!(waited.value["settled"], true, "{}", waited.value);

    let shot = call(
        &world,
        "screenshot",
        json!({"max_width": 160, "max_height": 100}),
    )
    .expect("a shot");
    assert_eq!(
        shot.value["cursor"],
        json!({"x": 0, "y": 0}),
        "the pointer starts at the origin"
    );
    let listed = call(&world, "displays", json!({})).expect("displays");
    let displays = listed.value["displays"].as_array().unwrap();
    assert_eq!(displays.len(), 2, "{}", listed.value);
    assert_eq!(displays[0]["primary"], true);
    assert_eq!(displays[0]["width"], 320);
    assert_eq!(displays[0]["last_screenshot"]["scale"], 0.5);
    assert_eq!(displays[1]["width"], 160);
    assert!(displays[1].get("last_screenshot").is_none());

    call(&world, "move_pointer", json!({"x": 40, "y": 30})).expect("a move");
    let at = call(&world, "cursor_position", json!({})).expect("the cursor");
    assert_eq!(at.value["display"], "0");
    assert_eq!(
        (at.value["x"].as_i64(), at.value["y"].as_i64()),
        (Some(80), Some(60))
    );
    assert_eq!(at.value["screenshot"], json!({"x": 40, "y": 30}));
}

#[test]
fn the_second_display_is_its_own_picture_and_its_own_scale() {
    let world = world(on(), all());
    let shot = call(
        &world,
        "screenshot",
        json!({"display": "1", "max_width": 80, "max_height": 80}),
    )
    .expect("display 1");
    assert_eq!(shot.value["display"], "1");
    assert_eq!(shot.value["scale"], 0.5);
    call(&world, "click", json!({"x": 10, "y": 10, "display": "1"})).expect("a click on it");
    assert_eq!(log(&world), ["click:leftx1@1:20,20"]);
    // The primary display was not shown, so it has no picture to take points from.
    assert_eq!(
        call(&world, "click", json!({"x": 1, "y": 1}))
            .unwrap_err()
            .code,
        "no_screenshot"
    );
}

#[test]
fn the_format_is_png_until_the_picture_is_larger_than_the_limit() {
    let world = world(on(), all());
    let png = call(&world, "screenshot", json!({})).expect("a screenshot");
    assert_eq!(png.images[0].mime, "image/png");
    let mut small = ToolSettings::default();
    small.gui_auto_limit = 64;
    let world = world_with(on(), all(), small);
    let auto = call(&world, "screenshot", json!({})).expect("auto");
    assert_eq!(auto.images[0].mime, "image/jpeg");
    assert_eq!(&auto.images[0].data[..2], b"\xff\xd8");
    assert_eq!(
        auto.value["settled"], true,
        "the settle of the first picture is kept"
    );
    let forced = call(&world, "screenshot", json!({"format": "png"})).expect("png");
    assert_eq!(forced.images[0].mime, "image/png");
    let jpeg = call(
        &world,
        "screenshot",
        json!({"format": "jpeg", "quality": 50}),
    )
    .expect("jpeg");
    assert_eq!(jpeg.images[0].mime, "image/jpeg");
}

#[test]
fn an_input_answers_with_a_settled_screenshot_when_the_tools_are_set_to() {
    let world = world_with(on(), all(), after_action());
    let typed = call(&world, "type", json!({"text": "x"})).expect("typed");
    assert_eq!(typed.images.len(), 1);
    assert_eq!(typed.value["screenshot"]["width"], 320);
    // The after-action picture is the one later points are taken from.
    call(&world, "click", json!({"x": 5, "y": 6})).expect("a click in it");
    assert_eq!(log(&world)[1], "click:leftx1@0:5,6");

    // An input-only grant types, and says why it saw nothing.
    let blind = world_with(
        on(),
        Grant::allowing([Permission::Info, Permission::GuiInput]),
        after_action(),
    );
    let typed = call(&blind, "type", json!({"text": "y"})).expect("typed");
    assert!(typed.images.is_empty());
    assert_eq!(
        typed.value["screenshot_error"]["code"], "denied",
        "{}",
        typed.value
    );
    assert_eq!(log(&blind), ["text:y"]);
}

#[test]
fn the_clipboard_needs_its_own_grant() {
    let world = world(on(), all());
    for (name, arguments) in [
        ("clipboard_read", json!({})),
        ("clipboard_write", json!({"text": "secret"})),
    ] {
        let error = call(&world, name, arguments).expect_err("no gui.clipboard");
        assert_eq!(error.code, "denied", "{name}: {error}");
        assert!(error.message.contains("gui.clipboard"), "{error}");
    }
    assert_eq!(world.desktop.clipboard(), "");
    // The refusal leaves the other tools working on the same link.
    assert!(call(&world, "screenshot", json!({})).is_ok());

    let granted = world_with(
        on(),
        Grant::allowing([
            Permission::Info,
            Permission::GuiView,
            Permission::GuiClipboard,
        ]),
        ToolSettings::default(),
    );
    call(&granted, "clipboard_write", json!({"text": "copied"})).expect("a write");
    assert_eq!(granted.desktop.clipboard(), "copied");
    let read = call(&granted, "clipboard_read", json!({})).expect("a read");
    assert_eq!(read.value["text"], "copied");

    // `gui.input` with the clipboard, and no `gui.view`.
    let driver = world_with(
        on(),
        Grant::allowing([
            Permission::Info,
            Permission::GuiInput,
            Permission::GuiClipboard,
        ]),
        ToolSettings::default(),
    );
    call(&driver, "clipboard_write", json!({"text": "typed"})).expect("a write beside input");
    assert_eq!(driver.desktop.clipboard(), "typed");
}

#[test]
fn a_watch_reports_what_changed_and_a_view_only_grant_cannot_drive() {
    let world = world(
        on(),
        Grant::allowing([Permission::Info, Permission::GuiView]),
    );
    let watched = call(&world, "gui_watch", json!({"duration_ms": 800})).expect("a watch");
    assert!(
        watched.value["updates"].as_u64().unwrap() >= 1,
        "{}",
        watched.value
    );
    assert_eq!(watched.value["width"], 320);
    for (name, arguments) in [
        ("type", json!({"text": "x"})),
        ("key", json!({"keys": "a"})),
        ("click", json!({"x": 1, "y": 1, "space": "display"})),
        ("scroll", json!({"dy": 1})),
        (
            "drag",
            json!({"from": {"x": 1, "y": 1}, "to": {"x": 2, "y": 2}, "space": "display"}),
        ),
    ] {
        let error = call(&world, name, arguments).expect_err("refused");
        assert_eq!(error.code, "denied", "{name}: {error}");
        assert!(error.message.contains("gui.input"), "{error}");
    }
    assert!(world.desktop.log().is_empty());
    assert!(call(&world, "screenshot", json!({})).is_ok());
}

#[test]
fn a_host_without_a_desktop_lists_the_tools_as_unavailable() {
    let world = world(GuiConfig::default(), all());
    let listed = world.tools.available();
    let gui: Vec<_> = listed.iter().filter(|(spec, _)| spec.is_gui()).collect();
    assert!(gui.len() >= 13, "{} desktop tools", gui.len());
    for (spec, availability) in gui {
        assert!(
            matches!(availability, Availability::Unavailable(_)),
            "{}: {availability:?}",
            spec.name
        );
    }
    let error = call(&world, "screenshot", json!({})).expect_err("unavailable");
    assert_eq!(error.code, "unavailable");
}
