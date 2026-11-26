//! Presence: the notice to the person at the screen and the approval they may be asked for, against a painted desktop and
//! a fake notifier. Nothing is shown on this machine.

#![forbid(unsafe_code)]

#[path = "../examples/support/painted.rs"]
mod painted;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{ApprovalMode, ControlConfig, GuiConfig, LinkConfig, NodeConfig, Span};
use bohio::gui::{Approval, ApprovalRequest, Decide, Engagement, Notice, Notifier, Presence};
use bohio::local::{Local, LocalLink};
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_client::Error;
use bohio_client::gui::{Gui, SnapshotOptions};
use bohio_client::link::{Settings, stream};
use bohio_proto::ErrorCode;
use bohio_proto::gui::{GuiRequest, Want};
use cabuya::auth::{Identity, Method};
use coqui_peer::PeerId;
use coqui_transport::{Channel, StreamChannel, memory};
use painted::Painted;

const WAIT: Duration = Duration::from_secs(30);

struct Host {
    node: Arc<Node>,
    id: PeerId,
    client: Arc<Node>,
    client_id: PeerId,
    grants: Arc<MemoryGrants>,
    link: PeerLink,
    channels: [Arc<StreamChannel>; 2],
    threads: Vec<JoinHandle<()>>,
}

impl Host {
    fn set_grant(&self, grant: Grant) {
        self.grants.set(self.client_id, grant);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        for channel in &self.channels {
            channel.close();
        }
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

fn host(gui: GuiConfig, configure: impl FnOnce(Node) -> Node) -> Host {
    let config = NodeConfig {
        link: LinkConfig {
            recheck: Span(Duration::from_millis(400)),
            ..LinkConfig::default()
        },
        gui,
        ..NodeConfig::default()
    };
    let make = |config: NodeConfig| {
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let (keys, grants) = (Arc::new(MemoryKeys::new()), Arc::new(MemoryGrants::new()));
        let node = Node::new(config, identity, keys.clone(), grants.clone()).expect("a node");
        (node, id, keys, grants)
    };
    let (node, id, keys, grants) = make(config);
    let node = configure(node.with_desktop(Arc::new(Painted::new())));
    let (client, client_id, client_keys, _) = make(NodeConfig::default());
    keys.accept(client_id);
    grants.set(
        client_id,
        Grant::allowing([Permission::GuiView, Permission::GuiInput]),
    );
    client_keys.accept(id);
    let (node, client) = (Arc::new(node), Arc::new(client));
    let settings = Settings::default();
    let (one, two) = memory::pipe(1024 * 1024);
    let dial_end = Arc::new(stream(one, &settings).expect("a channel"));
    let answer_end = Arc::new(stream(two, &settings).expect("a channel"));
    let threads = vec![
        thread::spawn({
            let (peer, channel) = (client.clone(), dial_end.clone());
            move || {
                let _ = peer.connect_channel(&*channel, &id, None);
                channel.close();
            }
        }),
        thread::spawn({
            let (peer, channel) = (node.clone(), answer_end.clone());
            move || {
                peer.serve_channel(&*channel, None);
                channel.close();
            }
        }),
    ];
    let until = Instant::now() + WAIT;
    let link = loop {
        if let Some(link) = client.peer(&id) {
            break link;
        }
        assert!(Instant::now() < until, "no link came up");
        thread::sleep(Duration::from_millis(20));
    };
    Host {
        node,
        id,
        client,
        client_id,
        grants,
        link,
        channels: [dial_end, answer_end],
        threads,
    }
}

fn config() -> GuiConfig {
    GuiConfig {
        enabled: true,
        indicator_linger: Span(Duration::from_millis(400)),
        // A stream that was just closed holds its slot until its engine next looks at the connection.
        max_streams: 32,
        max_streams_per_peer: 32,
        ..GuiConfig::default()
    }
}

#[derive(Default)]
struct Recording {
    engaged: Mutex<Vec<Engagement>>,
    released: Mutex<Vec<Engagement>>,
    asked: AtomicUsize,
    answer: Option<Approval>,
}

impl Presence for Recording {
    fn engaged(&self, engagement: &Engagement) {
        self.engaged
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(engagement.clone());
    }

    fn released(&self, engagement: &Engagement) {
        self.released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(engagement.clone());
    }

    fn approve(&self, _request: &ApprovalRequest<'_>) -> Approval {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.answer.unwrap_or(Approval::Deny)
    }
}

fn count(list: &Mutex<Vec<Engagement>>) -> usize {
    list.lock().unwrap_or_else(PoisonError::into_inner).len()
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let limit = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < limit, "{what}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn ten_short_streams_make_one_engagement_released_after_the_linger() {
    let presence = Arc::new(Recording::default());
    let host = host(config(), |node| node.with_presence(presence.clone()));
    for _ in 0..10 {
        let gui = Gui::open(&host.link, &GuiRequest::default()).unwrap();
        gui.snapshot(&SnapshotOptions::default(), Some(WAIT))
            .unwrap();
        gui.close();
    }
    assert_eq!(count(&presence.engaged), 1);
    assert_eq!(
        count(&presence.released),
        0,
        "released before the linger passed"
    );
    until("the engagement was never released", || {
        count(&presence.released) == 1
    });
    assert_eq!(count(&presence.engaged), 1);
    let released = presence.released.lock().unwrap()[0].clone();
    assert_eq!(released.peer, host.client_id);
    assert!(!released.controlling);
    assert_eq!(presence.asked.load(Ordering::SeqCst), 0);
}

#[test]
fn starting_to_control_while_viewing_is_told_again() {
    let presence = Arc::new(Recording::default());
    let host = host(config(), |node| node.with_presence(presence.clone()));
    let _view = Gui::open(&host.link, &GuiRequest::default()).unwrap();
    let _drive = Gui::open(&host.link, &GuiRequest::new([Want::View, Want::Input])).unwrap();
    let told: Vec<bool> = presence
        .engaged
        .lock()
        .unwrap()
        .iter()
        .map(|engagement| engagement.controlling)
        .collect();
    assert_eq!(told, [false, true]);
}

#[test]
fn the_indicator_can_be_switched_off() {
    let presence = Arc::new(Recording::default());
    let off = GuiConfig {
        indicator: bohio::config::Indicator::None,
        ..config()
    };
    let host = host(off, |node| node.with_presence(presence.clone()));
    let gui = Gui::open(&host.link, &GuiRequest::default()).unwrap();
    gui.close();
    thread::sleep(Duration::from_millis(900));
    assert_eq!(count(&presence.engaged) + count(&presence.released), 0);
}

#[test]
fn a_presence_that_always_denies_does_not_gate_a_host_with_no_approval() {
    let presence = Arc::new(Recording {
        answer: Some(Approval::Deny),
        ..Recording::default()
    });
    let host = host(config(), |node| node.with_presence(presence.clone()));
    let gui = Gui::open(&host.link, &GuiRequest::default()).unwrap();
    assert!(
        gui.snapshot(&SnapshotOptions::default(), Some(WAIT))
            .is_ok()
    );
    assert_eq!(presence.asked.load(Ordering::SeqCst), 0);
}

#[test]
fn an_application_presence_is_asked_when_approval_is_local() {
    let presence = Arc::new(Recording {
        answer: Some(Approval::Allow),
        ..Recording::default()
    });
    let local = GuiConfig {
        approval: ApprovalMode::Local,
        ..config()
    };
    let host = host(local, |node| node.with_presence(presence.clone()));
    assert!(Gui::open(&host.link, &GuiRequest::default()).is_ok());
    assert!(Gui::open(&host.link, &GuiRequest::default()).is_ok());
    assert_eq!(
        presence.asked.load(Ordering::SeqCst),
        1,
        "a second stream inside the ttl was asked"
    );
}

/// Notices as the fake notifier saw them, with the decision of each that asked.
#[derive(Default)]
struct Notices {
    shown: Mutex<Vec<Notice>>,
    deciders: Mutex<Vec<Decide>>,
}

impl Notifier for Notices {
    fn notify(&self, notice: &Notice, decide: Option<Decide>) {
        self.shown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(notice.clone());
        if let Some(decide) = decide {
            self.deciders
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(decide);
        }
    }
}

#[test]
fn the_notice_names_the_peer_by_twelve_digits_and_says_nothing_else() {
    let notices = Arc::new(Notices::default());
    let host = host(config(), |node| node.with_notifier(notices.clone()));
    let text = host.client_id.to_string();
    let (scheme, digits) = text.split_once(':').unwrap();
    let short = format!("{scheme}:{}", &digits[..12]);
    {
        let _gui = Gui::open(&host.link, &GuiRequest::new([Want::View, Want::Input])).unwrap();
        until("no notice", || !notices.shown.lock().unwrap().is_empty());
    }
    until("no stop notice", || {
        notices.shown.lock().unwrap().len() == 2
    });
    let shown = notices.shown.lock().unwrap().clone();
    assert_eq!(shown[0].title, "Bohío");
    assert_eq!(
        shown[0].body,
        format!("peer {short} is controlling this screen")
    );
    assert_eq!(
        shown[1].body,
        format!("peer {short} has stopped controlling this screen")
    );
    assert!(!shown[0].body.contains(digits));
}

fn local_endpoint(host: &Host, tag: &str) -> (Arc<AtomicBool>, JoinHandle<()>, LocalLink) {
    let control = ControlConfig {
        endpoint: Some(format!(
            "pipe://bohio-gui-test-{}-{tag}",
            std::process::id()
        )),
        ..ControlConfig::default()
    };
    let local = Local::bind(&control, &host.id).expect("the local endpoint binds");
    let endpoint = local.endpoint().clone();
    let stop = Arc::new(AtomicBool::new(false));
    let serving = thread::spawn({
        let (node, stop) = (host.node.clone(), stop.clone());
        move || {
            let _ = node.serve_local(&local, &stop);
        }
    });
    let link = LocalLink::connect(&endpoint, &Settings::default()).expect("a local link");
    (stop, serving, link)
}

fn open_in_thread(link: &PeerLink) -> JoinHandle<Result<Gui, Error>> {
    let link = link.clone();
    thread::spawn(move || Gui::open(&link, &GuiRequest::default()))
}

#[test]
fn local_approval_waits_for_the_owner_and_the_command_decides() {
    let gui = GuiConfig {
        approval: ApprovalMode::Local,
        approval_timeout: Span(Duration::from_millis(1500)),
        ..config()
    };
    let host = host(gui, |node| node);
    let (stop, serving, local) = local_endpoint(&host, "decide");

    // Allow: the stream waits, then opens.
    let waiting = open_in_thread(&host.link);
    until("nothing was waiting", || {
        !local.gui_pending().unwrap().is_empty()
    });
    assert!(
        !waiting.is_finished(),
        "the stream opened before anyone allowed it"
    );
    let pending = local.gui_pending().unwrap();
    assert_eq!(pending[0].peer, host.client_id.to_string());
    assert_eq!(pending[0].want, [Want::View]);
    assert!(local.gui_decide(pending[0].id, true).unwrap());
    let opened = waiting.join().unwrap().expect("allowed");
    assert!(
        opened
            .snapshot(&SnapshotOptions::default(), Some(WAIT))
            .is_ok()
    );
    // A second stream inside approval_ttl is not asked.
    let again = Gui::open(&host.link, &GuiRequest::default()).expect("not asked again");
    assert!(local.gui_pending().unwrap().is_empty());
    drop((again, opened));
    assert!(
        !local.gui_decide(999, true).unwrap(),
        "an unknown request was found"
    );

    stop.store(true, Ordering::SeqCst);
    serving.join().unwrap();
}

#[test]
fn denying_and_the_timeout_are_not_approved_and_the_notice_action_allows() {
    let gui = GuiConfig {
        approval: ApprovalMode::Local,
        approval_timeout: Span(Duration::from_millis(1000)),
        approval_ttl: Span(Duration::from_millis(1)),
        ..config()
    };
    let notices = Arc::new(Notices::default());
    let host = host(gui, |node| node.with_notifier(notices.clone()));
    let (stop, serving, local) = local_endpoint(&host, "deny");

    let waiting = open_in_thread(&host.link);
    until("nothing was waiting", || {
        !local.gui_pending().unwrap().is_empty()
    });
    local
        .gui_decide(local.gui_pending().unwrap()[0].id, false)
        .unwrap();
    let Err(Error::Remote(body)) = waiting.join().unwrap() else {
        panic!("a denied stream opened");
    };
    assert_eq!(
        (body.code, body.message.as_str()),
        (ErrorCode::Denied, "not approved")
    );

    let started = Instant::now();
    let Err(Error::Remote(body)) = open_in_thread(&host.link).join().unwrap() else {
        panic!("an unanswered stream opened");
    };
    assert_eq!(body.message, "not approved");
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert!(
        local.gui_pending().unwrap().is_empty(),
        "a timed out request stayed listed"
    );

    // The notice of a request that asks carries the choice, and the choice works.
    notices.deciders.lock().unwrap().clear();
    let waiting = open_in_thread(&host.link);
    until("the notice carried no action", || {
        !notices.deciders.lock().unwrap().is_empty()
    });
    let decide = notices.deciders.lock().unwrap().pop().unwrap();
    decide(true);
    assert!(waiting.join().unwrap().is_ok());
    let asking = notices
        .shown
        .lock()
        .unwrap()
        .iter()
        .any(|notice| notice.body.ends_with("asks to view this screen"));
    assert!(asking);

    stop.store(true, Ordering::SeqCst);
    serving.join().unwrap();
    let _ = host.client.links();
}

#[test]
fn an_approval_covers_what_was_approved_and_not_more() {
    let presence = Arc::new(Recording {
        answer: Some(Approval::Allow),
        ..Recording::default()
    });
    let local = GuiConfig {
        approval: ApprovalMode::Local,
        ..config()
    };
    let host = host(local, |node| node.with_presence(presence.clone()));
    host.set_grant(Grant::allowing([
        Permission::GuiView,
        Permission::GuiInput,
        Permission::GuiClipboard,
    ]));
    let asked = || presence.asked.load(Ordering::SeqCst);
    assert!(Gui::open(&host.link, &GuiRequest::new([Want::View])).is_ok());
    assert_eq!(asked(), 1);
    // The same again, and a part of what was allowed, are not asked.
    assert!(Gui::open(&host.link, &GuiRequest::new([Want::View])).is_ok());
    assert_eq!(asked(), 1, "what was allowed was asked again");
    // Control and the clipboard were never allowed: each is asked.
    assert!(Gui::open(&host.link, &GuiRequest::new([Want::View, Want::Input])).is_ok());
    assert_eq!(
        asked(),
        2,
        "consent to view was spent as consent to control"
    );
    assert!(Gui::open(&host.link, &GuiRequest::new([Want::View, Want::Clipboard])).is_ok());
    assert_eq!(
        asked(),
        3,
        "consent to view was spent as consent to the clipboard"
    );
    // What has now been allowed is not asked again.
    assert!(Gui::open(&host.link, &GuiRequest::new([Want::Input, Want::Clipboard])).is_ok());
    assert_eq!(asked(), 3);
}

#[test]
fn the_asking_notice_names_the_clipboard() {
    let peer = Identity::generate(Method::DEFAULT)
        .expect("an identity")
        .peer_id();
    let body = |want: &[Want]| bohio::gui::presence::asking_notice(&peer, want).body;
    assert!(body(&[Want::View]).ends_with("asks to view this screen"));
    assert!(
        body(&[Want::View, Want::Clipboard])
            .ends_with("asks to view this screen and its clipboard")
    );
    assert!(body(&[Want::Input]).ends_with("asks to control this screen"));
}

#[test]
fn a_peer_that_keeps_asking_is_denied_without_the_person_being_asked() {
    let presence = Arc::new(Recording {
        answer: Some(Approval::Allow),
        ..Recording::default()
    });
    let local = GuiConfig {
        approval: ApprovalMode::Local,
        approval_ttl: Span(Duration::ZERO),
        approval_asks_per_minute: 3,
        ..config()
    };
    let host = host(local, |node| node.with_presence(presence.clone()));
    for _ in 0..3 {
        assert!(Gui::open(&host.link, &GuiRequest::default()).is_ok());
    }
    assert_eq!(presence.asked.load(Ordering::SeqCst), 3);
    let error = Gui::open(&host.link, &GuiRequest::default()).unwrap_err();
    assert!(
        matches!(&error, Error::Remote(body) if body.code == ErrorCode::Denied && body.message == "not approved"),
        "{error:?}"
    );
    assert_eq!(
        presence.asked.load(Ordering::SeqCst),
        3,
        "the fourth ask reached the person"
    );
}
