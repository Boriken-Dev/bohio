//! `bohio-power/1` and `bohio-media/1` served by a node over memory pipes, against in-memory sources: nothing here touches a
//! BMC, a hypervisor or a real machine.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::access::Sources;
use bohio::config::{AccessConfig, MediaConfig, NodeConfig, PowerConfig, SourceTables, Span};
use bohio::link::{NodeEvent, NodeEvents};
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_api::testing;
use bohio_client::Error;
use bohio_client::link::{Settings, stream};
use bohio_proto::media::Image;
use bohio_proto::power::Action;
use bohio_proto::{ErrorBody, ErrorCode};
use cabuya::auth::{Identity, Method};
use coqui_transport::{Channel, StreamChannel, memory};
use sha2::{Digest, Sha256};

const WAIT: Duration = Duration::from_secs(30);

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
    link: PeerLink,
    events: Arc<Recorder>,
    _wire: Wire,
}

fn tables(name: &str) -> SourceTables {
    #[derive(serde::Deserialize)]
    struct Whole {
        source: SourceTables,
    }
    toml::from_str::<Whole>(&format!(
        "[source.bmc]\nprovider = \"fake_power\"\nlink = \"{name}\"\n[source.disc]\nprovider = \"fake_media\"\nlink = \"{name}\"\n"
    ))
    .expect("source tables")
    .source
}

fn world(name: &str, power: bool, media: bool, timeout: Duration, grant: Grant) -> World {
    world_limited(name, power, media, timeout, 4, grant)
}

fn world_limited(
    name: &str,
    power: bool,
    media: bool,
    timeout: Duration,
    max_streams: usize,
    grant: Grant,
) -> World {
    let config = NodeConfig {
        power: PowerConfig {
            enabled: power,
            action_timeout: Span(timeout),
            max_streams,
            ..PowerConfig::default()
        },
        media: MediaConfig {
            enabled: media,
            action_timeout: Span(timeout),
            max_streams,
            ..MediaConfig::default()
        },
        ..NodeConfig::default()
    };
    let sources = Sources::unconfigured();
    sources
        .register_library("bohio-api-testing", testing::library)
        .expect("the fake providers register");
    sources
        .reconfigure(
            &tables(name),
            &AccessConfig {
                power: vec!["bmc".to_owned()],
                media: vec!["disc".to_owned()],
                ..AccessConfig::default()
            },
        )
        .expect("the sources build");
    let events = Arc::new(Recorder::default());
    let make = |config: NodeConfig, sources: Option<Sources>| {
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let mut node =
            Node::new(config, identity, keys.clone(), grants.clone()).expect("the node is built");
        if let Some(sources) = sources {
            node = node.with_sources(sources);
        }
        (Arc::new(node), id, keys, grants)
    };
    let (host, host_id, host_keys, host_grants) = make(config, Some(sources));
    let host = {
        // The events are the host's.
        let node = Arc::into_inner(host).expect("one owner");
        Arc::new(node.with_events(events.clone()))
    };
    let (client, client_id, client_keys, _) = make(NodeConfig::default(), None);
    host_keys.accept(client_id);
    host_grants.set(client_id, grant);
    client_keys.accept(host_id);
    let settings = Settings::default();
    let (one, two) = memory::pipe(1024 * 1024);
    let dial_end = Arc::new(stream(one, &settings).expect("a channel"));
    let answer_end = Arc::new(stream(two, &settings).expect("a channel"));
    let threads = vec![
        thread::spawn({
            let (node, channel) = (client.clone(), dial_end.clone());
            move || {
                let _ = node.connect_channel(&*channel, &host_id, None);
                channel.close();
            }
        }),
        thread::spawn({
            let (node, channel) = (host.clone(), answer_end.clone());
            move || {
                node.serve_channel(&*channel, None);
                channel.close();
            }
        }),
    ];
    let until = Instant::now() + WAIT;
    let link = loop {
        if let Some(link) = client.peer(&host_id) {
            break link;
        }
        assert!(Instant::now() < until, "no link came up");
        thread::sleep(Duration::from_millis(20));
    };
    World {
        link,
        events,
        _wire: Wire {
            channels: [dial_end, answer_end],
            threads,
        },
    }
}

fn can(permission: Permission) -> Grant {
    Grant::allowing([Permission::Info, permission])
}

fn remote(error: &Error) -> &ErrorBody {
    match error {
        Error::Remote(body) => body,
        other => panic!("expected the host's error, got {other:?}"),
    }
}

/// A refusal at the link (the peer lacks the permission the protocol needs) or by the engine.
fn refused(error: &Error, code: ErrorCode) -> bool {
    matches!(error, Error::Transport(_))
        || matches!(error, Error::Remote(body) if body.code == code)
}

#[test]
fn every_power_action_reaches_the_source_and_its_answer_the_client() {
    let link = testing::power_link("pm-actions");
    let world = world(
        "pm-actions",
        true,
        true,
        Duration::from_secs(30),
        can(Permission::Power),
    );
    let status = bohio_client::power::status(&world.link, None, Some(WAIT)).expect("status");
    assert_eq!(status.state, "off");
    let mut seen = Vec::new();
    for action in [
        Action::On,
        Action::Cycle,
        Action::Reset,
        Action::Nmi,
        Action::SoftOff,
        Action::Off,
    ] {
        let state = bohio_client::power::act(&world.link, Some("bmc"), action, Some(WAIT))
            .expect("the action");
        assert!(state.done);
        seen.push(action.name());
    }
    assert_eq!(link.actions(), seen);
    assert_eq!(link.state(), "off");
    let events = world.events.0.lock().unwrap().clone();
    assert!(
        events.iter().any(|event| matches!(event, NodeEvent::Power { source, action, .. } if source == "bmc" && action == "cycle")),
        "the owner hears who did what: {events:?}"
    );
}

#[test]
fn a_peer_without_power_is_denied_and_a_node_with_it_off_says_unsupported() {
    let link = testing::power_link("pm-denied");
    let world = world(
        "pm-denied",
        true,
        true,
        Duration::from_secs(30),
        can(Permission::Media),
    );
    let error = bohio_client::power::act(&world.link, None, Action::Off, Some(WAIT))
        .expect_err("no power permission");
    assert!(refused(&error, ErrorCode::Denied), "{error:?}");
    assert!(link.actions().is_empty(), "nothing reached the source");

    let off = world_off();
    let error = bohio_client::power::status(&off.link, None, Some(WAIT)).expect_err("off");
    assert_eq!(remote(&error).code, ErrorCode::Unsupported);
}

fn world_off() -> World {
    world(
        "pm-off",
        false,
        false,
        Duration::from_secs(30),
        can(Permission::Power),
    )
}

#[test]
fn a_source_slower_than_the_action_timeout_is_a_timeout() {
    let link = testing::power_link("pm-slow");
    link.set_delay(Duration::from_secs(3));
    let world = world(
        "pm-slow",
        true,
        true,
        Duration::from_millis(300),
        can(Permission::Power),
    );
    let started = Instant::now();
    let error =
        bohio_client::power::act(&world.link, None, Action::On, Some(WAIT)).expect_err("too slow");
    assert_eq!(remote(&error).code, ErrorCode::Timeout);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the host did not wait for the source"
    );
}

#[test]
fn a_call_that_timed_out_keeps_its_slot_until_the_source_returns() {
    let link = testing::power_link("pm-held");
    link.set_delay(Duration::from_secs(3));
    let world = world_limited(
        "pm-held",
        true,
        true,
        Duration::from_millis(200),
        1,
        can(Permission::Power),
    );
    let first =
        bohio_client::power::act(&world.link, None, Action::On, Some(WAIT)).expect_err("too slow");
    assert_eq!(remote(&first).code, ErrorCode::Timeout);
    // The first call still runs in the source: its slot is not free, so a second call is turned away and starts no thread.
    let second = bohio_client::power::act(&world.link, None, Action::Off, Some(WAIT))
        .expect_err("the slot is held");
    assert_eq!(remote(&second).code, ErrorCode::Busy, "{second:?}");
    // The slot comes back once the source returns.
    let until = Instant::now() + WAIT;
    loop {
        link.set_delay(Duration::ZERO);
        match bohio_client::power::status(&world.link, None, Some(WAIT)) {
            Ok(_) => break,
            Err(error) => assert!(
                Instant::now() < until && remote(&error).code == ErrorCode::Busy,
                "{error:?}"
            ),
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn media_is_inserted_listed_and_ejected_by_url_and_the_event_names_no_image() {
    let world = world(
        "pm-url",
        true,
        true,
        Duration::from_secs(30),
        can(Permission::Media),
    );
    let slots = bohio_client::media::insert(
        &world.link,
        None,
        "cd",
        Image::Url {
            url: "https://images.invalid/installer.iso?token=hunter2".to_owned(),
        },
        Some(WAIT),
    )
    .expect("inserted");
    assert!(slots[0].inserted);
    assert!(
        !slots[0]
            .image
            .clone()
            .unwrap_or_default()
            .contains("hunter2"),
        "the source shows no query"
    );
    let listed = bohio_client::media::status(&world.link, None, Some(WAIT)).expect("status");
    assert!(listed.iter().any(|slot| slot.slot == "cd" && slot.inserted));
    let ejected = bohio_client::media::eject(&world.link, None, "cd", Some(WAIT)).expect("ejected");
    assert!(!ejected[0].inserted);
    let events = format!("{:?}", world.events.0.lock().unwrap());
    assert!(
        !events.contains("hunter2") && !events.contains("installer"),
        "{events}"
    );
}

#[test]
fn an_image_of_the_node_is_read_whole_through_the_roots_and_one_outside_is_refused() {
    let dir = tempfile::tempdir().expect("a directory");
    let root = dir.path().join("images");
    std::fs::create_dir_all(&root).unwrap();
    let image: Vec<u8> = (0..200_000_u32).map(|n| (n % 251) as u8).collect();
    std::fs::write(root.join("installer.iso"), &image).unwrap();
    std::fs::write(dir.path().join("secret.iso"), b"outside").unwrap();
    let grant = Grant::allowing([Permission::Info, Permission::Media, Permission::FsRead])
        .with_fs_roots([root.clone()]);
    let link = testing::media_link("pm-path");
    let world = world("pm-path", true, true, Duration::from_secs(30), grant);

    let slots = bohio_client::media::insert(
        &world.link,
        None,
        "cd",
        Image::Path {
            path: root.join("installer.iso").to_string_lossy().into_owned(),
        },
        Some(WAIT),
    )
    .expect("an image under the root is inserted");
    assert_eq!(slots[0].size, Some(image.len() as u64));
    let read = link.read_of("cd").expect("the source read the image");
    assert_eq!(
        Sha256::digest(&read),
        Sha256::digest(&image),
        "the whole file, intact"
    );

    let outside = bohio_client::media::insert(
        &world.link,
        None,
        "floppy",
        Image::Path {
            path: dir.path().join("secret.iso").to_string_lossy().into_owned(),
        },
        Some(WAIT),
    )
    .expect_err("outside the roots");
    assert_eq!(remote(&outside).code, ErrorCode::OutsideRoot);
    assert!(link.read_of("floppy").is_none());

    // Without fs.read the path is refused even inside the root.
    let narrow = world_media_only(&root);
    let error = bohio_client::media::insert(
        &narrow.link,
        None,
        "cd",
        Image::Path {
            path: root.join("installer.iso").to_string_lossy().into_owned(),
        },
        Some(WAIT),
    )
    .expect_err("fs.read is needed too");
    assert_eq!(remote(&error).code, ErrorCode::Denied);
}

fn world_media_only(root: &std::path::Path) -> World {
    let grant =
        Grant::allowing([Permission::Info, Permission::Media]).with_fs_roots([root.to_path_buf()]);
    world("pm-path-narrow", true, true, Duration::from_secs(30), grant)
}

#[test]
fn media_off_or_not_granted_is_refused() {
    let off = world_off();
    let error = bohio_client::media::status(&off.link, None, Some(WAIT)).expect_err("off");
    assert!(refused(&error, ErrorCode::Unsupported), "{error:?}");
    let world = world(
        "pm-media-denied",
        true,
        true,
        Duration::from_secs(30),
        can(Permission::Power),
    );
    let error = bohio_client::media::status(&world.link, None, Some(WAIT))
        .expect_err("no media permission");
    assert!(refused(&error, ErrorCode::Denied), "{error:?}");
}

#[test]
fn info_lists_only_the_capabilities_whose_permission_the_peer_holds() {
    let _link = testing::power_link("pm-info");
    let sources = Sources::unconfigured();
    sources
        .register_library("bohio-api-testing", testing::library)
        .expect("the fake providers register");
    sources
        .reconfigure(
            &tables("pm-info"),
            &AccessConfig {
                power: vec!["bmc".to_owned()],
                media: vec!["disc".to_owned()],
                ..AccessConfig::default()
            },
        )
        .expect("the sources build");
    let defaults = ["bmc", "disc"];
    let only_info = Grant::allowing([Permission::Info]).with_default_sources(defaults);
    let told = sources.info(&only_info);
    assert!(told.power.is_empty(), "{:?}", told.power);
    assert!(told.media.is_empty(), "{:?}", told.media);

    let power =
        Grant::allowing([Permission::Info, Permission::Power]).with_default_sources(defaults);
    let told = sources.info(&power);
    assert_eq!(told.power.len(), 1);
    assert!(told.media.is_empty(), "{:?}", told.media);
}
