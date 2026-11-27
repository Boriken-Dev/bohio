//! A node serving the Windows desktop backend over memory pipes: what `info` says of it, and what a peer is told while the
//! secure desktop has the screen. Nothing here captures or drives an unlocked screen.

#![cfg(windows)]
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{GuiConfig, LinkConfig, NodeConfig, Span};
use bohio::gui::{Desktop, WindowsDesktop, WindowsSettings};
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_client::Error;
use bohio_client::control::Control;
use bohio_client::gui::{Gui, Snapshot};
use bohio_client::link::{Settings, stream};
use bohio_proto::ErrorCode;
use bohio_proto::gui::{GuiRequest, InputEvent, Rect, Want};
use cabuya::auth::{Identity, Method};
use coqui_peer::PeerId;
use coqui_transport::{Channel, StreamChannel, memory};

const WAIT: Duration = Duration::from_secs(30);

struct Peer {
    node: Arc<Node>,
    id: PeerId,
    keys: Arc<MemoryKeys>,
    grants: Arc<MemoryGrants>,
}

impl Peer {
    fn new(gui: GuiConfig, desktop: Option<Arc<WindowsDesktop>>) -> Arc<Self> {
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

fn world(gui: GuiConfig, grant: Grant) -> World {
    let host = Peer::new(
        gui,
        Some(Arc::new(WindowsDesktop::new(WindowsSettings::default()))),
    );
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

fn enabled() -> GuiConfig {
    GuiConfig {
        enabled: true,
        ..GuiConfig::default()
    }
}

#[test]
fn info_names_the_windows_backend_and_the_session_it_serves() {
    let grant = Grant::allowing([Permission::Info, Permission::GuiView]);
    let world = world(enabled(), grant);
    let info = Control::open(&world.link)
        .unwrap()
        .info(Some(WAIT))
        .unwrap();
    assert!(info.gui.enabled);
    assert_eq!(info.gui.backend.as_deref(), Some("windows"));
    assert!(info.gui.input && info.gui.clipboard && info.gui.lock_detection);
    match WindowsDesktop::new(WindowsSettings::default()).state() {
        Ok(state) => {
            assert!(info.gui.available, "{:?}", info.gui.reason);
            assert_eq!(info.gui.sessions.len(), 1);
            let session = &info.gui.sessions[0];
            assert_eq!(session.displays, state.displays);
            assert!(session.user.is_some());
        }
        Err(error) => {
            assert!(!info.gui.available);
            assert_eq!(info.gui.reason.as_deref(), Some(error.message.as_str()));
        }
    }
}

#[test]
fn while_the_secure_desktop_has_the_screen_a_peer_is_told_so() {
    let desktop = WindowsDesktop::new(WindowsSettings::default());
    let Ok(state) = desktop.state() else { return };
    if state.locked != Some(true) {
        println!(
            "NOT LOCKED: the secure-desktop answers through a node were not exercised on this screen"
        );
        return;
    }
    let grant = Grant::allowing([Permission::Info, Permission::GuiView, Permission::GuiInput]);
    // The host would type into a lock screen with `input_when_locked`; the backend still refuses.
    let world = world(
        GuiConfig {
            input_when_locked: true,
            ..enabled()
        },
        grant,
    );
    let gui = Gui::open(&world.link, &GuiRequest::new([Want::View, Want::Input])).unwrap();
    assert_eq!(gui.state().locked, Some(true));
    let options = Snapshot {
        region: Some(Rect::new(0, 0, 4, 4)),
        ..Snapshot::default()
    };
    let Err(Error::Remote(body)) = gui.snapshot(&options, Some(WAIT)) else {
        panic!("a snapshot of the lock screen was not refused");
    };
    assert_eq!(body.code, ErrorCode::Unavailable);
    assert!(body.message.contains("secure desktop"), "{}", body.message);
    let Err(Error::Remote(body)) = gui.input(
        &[InputEvent::Move {
            display: None,
            x: 1,
            y: 1,
        }],
        Some(WAIT),
    ) else {
        panic!("input to the lock screen was not refused");
    };
    assert_eq!(body.code, ErrorCode::Unavailable);
    assert!(body.message.contains("secure desktop"), "{}", body.message);
    println!(
        "LOCKED: a snapshot and input through a node were refused with the secure desktop's reason"
    );
}
