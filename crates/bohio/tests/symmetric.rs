//! Either side opens streams on any link: nodes linked in process over memory pipes, one dialing and
//! the other answering, each deciding what the other may do by its own grants.

#![forbid(unsafe_code)]

mod support;

use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{LinkConfig, NodeConfig, Span};
use bohio::node::{
    Keys, MemoryGrants, MemoryKeys, Node, NodeError, NodeEvent, NodeEvents, PeerLink, Side,
};
use bohio::policy::{Grant, Permission};
use bohio_client::Error;
use bohio_client::control::Control;
use bohio_client::exec::{Event as ExecEvent, Exec};
use bohio_client::fs::Reader;
use bohio_client::link::{Settings, stream};
use bohio_proto::exec::Request;
use bohio_proto::fs::ReadRequest;
use cabuya::auth::{Identity, Method};
use coqui_peer::{KeyMaterial, PeerId};
use coqui_transport::{Channel, Failure, StreamChannel, memory};
use support::{alive, gone_within, helper};

const WAIT: Duration = Duration::from_secs(30);
const RECHECK: Duration = Duration::from_millis(500);
const SLACK: Duration = Duration::from_secs(3);

/// The events a node told, in order.
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

impl Recorder {
    fn seen(&self) -> Vec<NodeEvent> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Accepted keys that also remember the strangers they heard.
#[derive(Default)]
struct Hearing {
    accepted: MemoryKeys,
    strangers: Mutex<Vec<PeerId>>,
}

impl Keys for Hearing {
    fn is_accepted(&self, peer: &PeerId) -> bool {
        self.accepted.is_accepted(peer)
    }

    fn stranger(&self, peer: &PeerId, _key: &KeyMaterial, _source: Option<std::net::SocketAddr>) {
        self.strangers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(*peer);
    }
}

/// One node with its own in-memory keys, grants and events.
struct Peer {
    node: Arc<Node>,
    id: PeerId,
    keys: Arc<Hearing>,
    grants: Arc<MemoryGrants>,
    events: Arc<Recorder>,
}

impl Peer {
    fn new() -> Arc<Self> {
        let config = NodeConfig {
            link: LinkConfig {
                recheck: Span(RECHECK),
                ..LinkConfig::default()
            },
            ..NodeConfig::default()
        };
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(Hearing::default());
        let grants = Arc::new(MemoryGrants::new());
        let events = Arc::new(Recorder::default());
        let node = Node::new(config, identity, keys.clone(), grants.clone())
            .expect("the node is built")
            .with_path(std::env::var_os("PATH").unwrap_or_default())
            .with_events(events.clone());
        Arc::new(Self {
            node: Arc::new(node),
            id,
            keys,
            grants,
            events,
        })
    }

    /// Accepts `other` and lets it do `grant`.
    fn trust(&self, other: &Self, grant: Grant) {
        self.keys.accepted.accept(other.id);
        self.grants.set(other.id, grant);
    }

    fn link_to(&self, other: &Self) -> PeerLink {
        let until = Instant::now() + WAIT;
        loop {
            if let Some(link) = self.node.peer(&other.id) {
                return link;
            }
            assert!(Instant::now() < until, "no link came up");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A link between two nodes over memory pipes, ended when dropped.
struct Wire {
    channels: [Arc<StreamChannel>; 2],
    dialed: Option<JoinHandle<Result<(), NodeError>>>,
    answered: Option<JoinHandle<()>>,
}

impl Wire {
    /// `dialer` dials `answerer`, expecting its key; the answerer serves what arrives.
    fn make(dialer: &Arc<Peer>, answerer: &Arc<Peer>) -> Self {
        let settings = Settings::default();
        let (one, two) = memory::pipe(1024 * 1024);
        let dial_end = Arc::new(stream(one, &settings).expect("a channel"));
        let answer_end = Arc::new(stream(two, &settings).expect("a channel"));
        let dialed = thread::spawn({
            let (peer, channel, expect) = (dialer.clone(), dial_end.clone(), answerer.id);
            move || {
                let result = peer.node.connect_channel(&*channel, &expect, None);
                channel.close();
                result
            }
        });
        let answered = thread::spawn({
            let (peer, channel) = (answerer.clone(), answer_end.clone());
            move || {
                peer.node.serve_channel(&*channel, None);
                channel.close();
            }
        });
        Self {
            channels: [dial_end, answer_end],
            dialed: Some(dialed),
            answered: Some(answered),
        }
    }

    /// What the dial returned, once the link has ended.
    fn dial_result(&mut self) -> Result<(), NodeError> {
        self.dialed
            .take()
            .expect("the dial is joined once")
            .join()
            .expect("the dial did not panic")
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        for channel in &self.channels {
            channel.close();
        }
        if let Some(handle) = self.dialed.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.answered.take() {
            let _ = handle.join();
        }
    }
}

fn exec_all(link: &PeerLink, args: &[&str]) -> String {
    let mut argv = vec![helper()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    let exec = Exec::start(link, &Request::argv(argv)).expect("the exec opens");
    let done = exec.wait(Some(WAIT)).expect("the exec ends");
    assert_eq!(done.exit.code, Some(0));
    String::from_utf8_lossy(&done.stdout).trim().to_owned()
}

/// Waits for `exec`'s end and returns the error that ended it, after how long.
fn ended_by(exec: &Exec) -> (Error, Duration) {
    let started = Instant::now();
    loop {
        match exec.next(Some(Duration::from_millis(100))) {
            Err(Error::Timeout) | Ok(Some(_)) => {}
            Ok(None) => panic!("the exec ended in order, not by a revoke"),
            Err(error) => return (error, started.elapsed()),
        }
        assert!(started.elapsed() < WAIT, "the exec was never ended");
    }
}

fn started_pid(exec: &Exec) -> u32 {
    let Ok(Some(ExecEvent::Started { pid })) = exec.next(Some(Duration::from_secs(20))) else {
        panic!("the process did not start");
    };
    assert!(alive(pid));
    pid
}

#[test]
fn a_host_that_dialed_is_controlled_by_the_controller_that_answered() {
    let (host, controller) = (Peer::new(), Peer::new());
    let dir = tempfile::tempdir().expect("a directory");
    std::fs::write(
        dir.path().join("hello.txt"),
        "read from the host that dialed\n",
    )
    .unwrap();
    host.trust(
        &controller,
        Grant::allowing([Permission::Info, Permission::Exec, Permission::FsRead])
            .with_fs_roots([dir.path()]),
    );
    // The controller hosts nothing: it only accepts the key.
    controller.trust(&host, Grant::none());
    let _wire = Wire::make(&host, &controller);

    let link = controller.link_to(&host);
    let info = Control::open(&link)
        .and_then(|control| control.info(Some(WAIT)))
        .expect("info");
    assert_eq!(info.host_id, Some(host.id.to_string()));
    let path = dir.path().join("hello.txt").to_string_lossy().into_owned();
    let read = Reader::start(&link, &ReadRequest::whole(path), Some(WAIT)).expect("the read opens");
    assert_eq!(
        read.read_to_end(Some(WAIT)).expect("the file"),
        b"read from the host that dialed\n"
    );
    assert_eq!(exec_all(&link, &["echo", "from", "host"]), "from host");

    let theirs = host.node.links();
    let ours = controller.node.links();
    assert_eq!((theirs.len(), ours.len()), (1, 1));
    assert_eq!(theirs[0].side, Side::Dialed);
    assert_eq!(ours[0].side, Side::Answered);
    assert_eq!((theirs[0].peer, ours[0].peer), (controller.id, host.id));
}

#[test]
fn both_sides_host_and_control_over_one_link_at_once() {
    let (one, other) = (Peer::new(), Peer::new());
    let grant = Grant::allowing([Permission::Exec]);
    one.trust(&other, grant.clone());
    other.trust(&one, grant);
    let _wire = Wire::make(&one, &other);

    let (to_other, to_one) = (one.link_to(&other), other.link_to(&one));
    thread::scope(|scope| {
        let first = scope.spawn(|| exec_all(&to_other, &["echo", "one", "ran", "it"]));
        let second = scope.spawn(|| exec_all(&to_one, &["echo", "other", "ran", "it"]));
        assert_eq!(first.join().unwrap(), "one ran it");
        assert_eq!(second.join().unwrap(), "other ran it");
    });
    assert_eq!(one.node.links().len(), 1);
    assert_eq!(other.node.links().len(), 1);
}

#[test]
fn a_dialed_key_the_node_does_not_accept_is_closed_after_the_proof_and_heard_as_a_stranger() {
    let (dialer, answerer) = (Peer::new(), Peer::new());
    // The answerer accepts the dialer and grants it everything; the dialer does not accept the answerer.
    answerer.trust(
        &dialer,
        Grant::allowing([Permission::Info, Permission::Exec]),
    );
    let mut wire = Wire::make(&dialer, &answerer);

    let result = wire.dial_result();
    assert_eq!(result, Err(NodeError::Stranger { peer: answerer.id }));
    assert_eq!(
        *dialer
            .keys
            .strangers
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
        [answerer.id]
    );
    assert!(dialer.events.seen().contains(&NodeEvent::Stranger {
        peer: answerer.id,
        source: None
    }));
    assert!(dialer.node.links().is_empty(), "a link was listed");
    assert!(dialer.node.peer(&answerer.id).is_none());
    // Nothing was served to the dialer: the answerer's link ends with the dialer's close.
    let until = Instant::now() + WAIT;
    while !answerer.node.links().is_empty() {
        assert!(Instant::now() < until, "the answerer's link did not end");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !answerer
            .events
            .seen()
            .iter()
            .any(|event| matches!(event, NodeEvent::Opened { .. })),
        "a stream was served"
    );
}

/// A long helper run on `host` for `controller`, and the revoke that ends it.
fn revoke_ends_a_running_exec(runs: &Arc<Peer>, hosts: &Arc<Peer>, link: &PeerLink) {
    let exec = Exec::start(link, &Request::argv([helper().as_str(), "tree", "120000"]))
        .expect("the exec opens");
    let pid = started_pid(&exec);
    hosts.grants.remove(&runs.id);
    let (error, took) = ended_by(&exec);
    assert!(
        matches!(error, Error::Transport(Failure::Permission)),
        "the stream was not ended by the admission: {error:?}"
    );
    assert!(took <= RECHECK * 2 + SLACK, "took {took:?}");
    assert!(
        gone_within(pid, Duration::from_secs(15)),
        "the process of a peer that lost its grant is still running"
    );
}

#[test]
fn revoking_a_grant_ends_a_running_exec_whichever_side_hosts() {
    let (one, other) = (Peer::new(), Peer::new());
    let grant = Grant::allowing([Permission::Exec]);
    one.trust(&other, grant.clone());
    other.trust(&one, grant);
    let _wire = Wire::make(&one, &other);

    // `one` dialed and runs on `other`; `other` takes the grant away.
    revoke_ends_a_running_exec(&one, &other, &one.link_to(&other));
    // `other` answered and runs on `one`; `one` takes the grant away.
    revoke_ends_a_running_exec(&other, &one, &other.link_to(&one));
}

/// `controller` opens an exec on `host`, which grants it nothing, over a link `dialer` made; the open is
/// refused, and the link still serves the opposite direction under `controller`'s own grant.
fn dialing_grants_nothing(
    dialer: &Arc<Peer>,
    answerer: &Arc<Peer>,
    controller: &Arc<Peer>,
    host: &Arc<Peer>,
) {
    host.trust(controller, Grant::none());
    controller.trust(host, Grant::none());
    let _wire = Wire::make(dialer, answerer);

    let link = controller.link_to(host);
    let refused = match Exec::start(&link, &Request::argv([helper().as_str(), "echo", "no"])) {
        Err(error) => error,
        Ok(exec) => exec.wait(Some(WAIT)).expect_err("the exec ran"),
    };
    assert!(
        matches!(refused, Error::Transport(Failure::Permission)),
        "{refused:?}"
    );
    // The host closes the stream before it reports the refusal, so the report may come a moment after.
    let until = Instant::now() + WAIT;
    while !host
        .events
        .seen()
        .iter()
        .any(|event| matches!(event, NodeEvent::Refused { .. }))
    {
        assert!(Instant::now() < until, "the host did not report a refusal");
        thread::sleep(Duration::from_millis(20));
    }

    controller
        .grants
        .set(host.id, Grant::allowing([Permission::Exec]));
    let back = host.link_to(controller);
    assert_eq!(exec_all(&back, &["echo", "reverse"]), "reverse");
}

#[test]
fn an_answering_host_grants_what_its_own_grants_say() {
    let (controller, host) = (Peer::new(), Peer::new());
    dialing_grants_nothing(&controller, &host, &controller, &host);
}

#[test]
fn a_dialing_host_grants_what_its_own_grants_say_and_dialing_grants_nothing() {
    let (controller, host) = (Peer::new(), Peer::new());
    dialing_grants_nothing(&host, &controller, &controller, &host);
}

#[test]
fn a_link_ends_when_its_channel_closes() {
    let (one, other) = (Peer::new(), Peer::new());
    one.trust(&other, Grant::none());
    other.trust(&one, Grant::none());
    let wire = Wire::make(&one, &other);
    let link = one.link_to(&other);
    assert!(link.is_up());
    drop(wire);
    assert!(!link.is_up());
    assert!(one.node.links().is_empty());
    assert!(matches!(
        Control::open(&link).err(),
        Some(Error::Closed | Error::Transport(_))
    ));
}

fn sorted(mut offers: Vec<String>) -> Vec<String> {
    offers.sort();
    offers
}

#[test]
fn offers_follow_the_grants_on_a_live_link() {
    let (host, controller) = (Peer::new(), Peer::new());
    host.trust(&controller, Grant::none());
    controller.trust(&host, Grant::none());
    let _wire = Wire::make(&controller, &host);
    let link = controller.link_to(&host);

    assert_eq!(
        link.offers(WAIT).expect("the document"),
        Vec::<String>::new()
    );
    host.grants.set(
        controller.id,
        Grant::allowing([Permission::Info, Permission::Exec]),
    );
    assert_eq!(
        sorted(link.offers(WAIT).expect("the document")),
        ["bohio-exec/1", "bohio/1"]
    );
    assert_eq!(
        sorted(host.node.links()[0].mine.clone()),
        ["bohio-exec/1", "bohio/1"]
    );
    host.grants.remove(&controller.id);
    assert_eq!(
        link.offers(WAIT).expect("the document"),
        Vec::<String>::new()
    );
    assert!(host.node.links()[0].mine.is_empty());
}

#[test]
fn a_node_granting_nothing_offers_nothing_and_still_controls() {
    let (controller, host) = (Peer::new(), Peer::new());
    host.trust(&controller, Grant::allowing([Permission::Exec]));
    controller.trust(&host, Grant::none());
    let _wire = Wire::make(&host, &controller);

    let to_host = controller.link_to(&host);
    let to_controller = host.link_to(&controller);
    assert_eq!(
        to_host.offers(WAIT).expect("the document"),
        ["bohio-exec/1"]
    );
    assert_eq!(
        to_controller.offers(WAIT).expect("the document"),
        Vec::<String>::new()
    );
    assert_eq!(
        exec_all(&to_host, &["echo", "still", "controls"]),
        "still controls"
    );
    assert!(controller.node.links()[0].mine.is_empty());
}

#[test]
fn an_unoffered_open_fails_with_a_permission_failure() {
    let (host, controller) = (Peer::new(), Peer::new());
    host.trust(&controller, Grant::allowing([Permission::Info]));
    controller.trust(&host, Grant::none());
    let _wire = Wire::make(&controller, &host);
    let link = controller.link_to(&host);

    assert_eq!(link.offers(WAIT).expect("the document"), ["bohio/1"]);
    let refused = match Exec::start(&link, &Request::argv([helper().as_str(), "echo", "no"])) {
        Err(error) => error,
        Ok(exec) => exec.wait(Some(WAIT)).expect_err("the exec ran"),
    };
    assert!(
        matches!(refused, Error::Transport(Failure::Permission)),
        "{refused:?}"
    );
}
