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
use bohio_client::tools::{Availability, Cancel, Output, ToolError, Tools};
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
    let tools = Tools::new(Arc::new(link)).expect("the tools are made");
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

#[test]
fn a_snapshot_is_an_image_that_fits_and_input_lands_on_the_desktop() {
    let world = world(on(), all());
    let shot = call(
        &world,
        "gui_snapshot",
        json!({"format": "png", "max_width": 160, "max_height": 100}),
    )
    .expect("a snapshot");
    assert_eq!(shot.images.len(), 1);
    assert_eq!(shot.images[0].mime, "image/png");
    assert_eq!(&shot.images[0].data[..4], b"\x89PNG");
    assert_eq!(shot.value["image"]["width"], 160, "{}", shot.value);
    assert_eq!(shot.value["image"]["scale"], 0.5);

    let typed = call(
        &world,
        "gui_input",
        json!({"text": "hello", "keys": "ctrl+s"}),
    )
    .expect("input");
    assert_eq!(typed.value["applied"], typed.value["sent"]);
    let log = world.desktop.log();
    assert!(log.iter().any(|line| line == "text:hello"), "{log:?}");
    assert!(
        log.len() >= 4,
        "the combination is control down, s, s up, control up: {log:?}"
    );

    let click = call(
        &world,
        "gui_input",
        json!({"events": [{"type": "button", "button": "left", "x": 10, "y": 10}]}),
    )
    .expect("a click");
    assert_eq!(click.value["applied"], 1);
    assert!(call(&world, "gui_input", json!({})).is_err());
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
    let error = call(&world, "gui_input", json!({"text": "x"})).expect_err("refused");
    assert_eq!(error.code, "denied", "{error}");
    assert!(error.message.contains("gui.input"), "{error}");
    assert!(world.desktop.log().is_empty());
    assert!(call(&world, "gui_snapshot", json!({})).is_ok());
}

#[test]
fn a_host_without_a_desktop_lists_the_tools_as_unavailable() {
    let world = world(GuiConfig::default(), all());
    let listed = world.tools.available();
    for name in ["gui_snapshot", "gui_input", "gui_watch"] {
        let entry = listed.iter().find(|(spec, _)| spec.name == name).unwrap();
        assert!(
            matches!(entry.1, Availability::Unavailable(_)),
            "{name}: {:?}",
            entry.1
        );
    }
    let error = call(&world, "gui_snapshot", json!({})).expect_err("unavailable");
    assert_eq!(error.code, "unavailable");
}
