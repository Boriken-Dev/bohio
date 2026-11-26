//! The per-user GUI helper: a node with no desktop of its own serves a helper's, over a real local pipe, to a peer that
//! reaches the node over memory channels. The helper's desktop is the painted in-memory one: no screen is touched.
//!
//! Both identity checks have a positive control (see the tests' names): the node closes a helper of an unlisted account
//! unread, and a helper refuses to serve a node whose account is not the one it was told.

#![forbid(unsafe_code)]

#[path = "../examples/support/painted.rs"]
mod painted;
mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::access::{Capability, DesktopOf, Sources};
use bohio::config::{GuiConfig, LinkConfig, NodeConfig, Span};
use bohio::gui::helper::{HelperListener, HelperSettings, resolve_account};
use bohio::gui::{Desktop, DesktopState, Frame, GuiError};
use bohio::link::GuiChange;
use bohio::node::{MemoryGrants, MemoryKeys, Node, NodeEvent, NodeEvents, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_client::Error;
use bohio_client::control::Control;
use bohio_client::dial::Endpoint;
use bohio_client::gui::{Gui, SnapshotOptions};
use bohio_client::link::{Settings, stream};
use bohio_proto::gui::{Button, ButtonAction, DisplayId, GuiRequest, InputEvent, Rect, Want};
use bohio_proto::{ErrorBody, ErrorCode};
use cabuya::auth::{Identity, Method};
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure, StreamChannel, memory};
use painted::{CLICK, Painted};

const WAIT: Duration = Duration::from_secs(30);
const RECHECK: Duration = Duration::from_millis(400);

/// The node's own desktop: there is none, as in a service's session.
struct Absent;

impl Desktop for Absent {
    fn state(&self) -> Result<DesktopState, GuiError> {
        Err(GuiError::unavailable(
            "session 0 has no desktop: run the node in the user's session, or use the GUI helper",
        ))
    }

    fn capture(&self, _: &DisplayId, _: Option<Rect>) -> Result<Frame, GuiError> {
        Err(GuiError::unavailable("no desktop"))
    }

    fn input(&self, _: &[InputEvent]) -> Result<u32, GuiError> {
        Err(GuiError::unavailable("no desktop"))
    }

    fn cursor(&self) -> Option<(DisplayId, i32, i32)> {
        None
    }

    fn clipboard_text(&self) -> Result<String, GuiError> {
        Err(GuiError::unavailable("no desktop"))
    }

    fn set_clipboard_text(&self, _: &str) -> Result<(), GuiError> {
        Err(GuiError::unavailable("no desktop"))
    }
}

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
    fn gui(&self) -> Vec<GuiChange> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|event| match event {
                NodeEvent::Gui { change, .. } => Some(change.clone()),
                _ => None,
            })
            .collect()
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
    fn new(gui: GuiConfig, desktop: Option<Arc<dyn Desktop>>) -> Arc<Self> {
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

/// This account as the system names it.
fn me() -> String {
    coqui_transport_pipe::current_user().expect("the current account")
}

/// An account that is not this one.
fn someone_else() -> String {
    if cfg!(windows) {
        "S-1-5-21-1-2-3-1001".to_owned()
    } else {
        let mine: u64 = me().parse().expect("a uid");
        (mine + 1).to_string()
    }
}

/// An endpoint no other test uses, in a directory only this account writes.
fn endpoint(dir: &tempfile::TempDir, name: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if cfg!(windows) {
        let next = NEXT.fetch_add(1, Ordering::SeqCst);
        format!("pipe://bohio-test-{name}-{}-{next}", std::process::id())
    } else {
        format!(
            "pipe://{}",
            dir.path().join(format!("{name}.sock")).display()
        )
    }
}

fn host_config(accounts: &[String], endpoint: &str, input_when_locked: bool) -> GuiConfig {
    GuiConfig {
        enabled: true,
        helper_accounts: accounts.to_vec(),
        helper_endpoint: Some(endpoint.to_owned()),
        input_when_locked,
        ..GuiConfig::default()
    }
}

fn sources_of(desktop: &Arc<Painted>) -> Arc<Sources> {
    Arc::new(
        Sources::unconfigured()
            .with_desktop("app", Arc::new(DesktopOf::new(desktop.clone())))
            .with_access(Capability::Desktop, ["app"]),
    )
}

fn helper_settings(endpoint: &str, node_account: &str, gui: GuiConfig) -> HelperSettings {
    HelperSettings {
        endpoint: Endpoint::parse(endpoint).expect("an endpoint"),
        node_account: node_account.to_owned(),
        session: "main".to_owned(),
        user: Some("tester".to_owned()),
        gui,
        link: Settings::default(),
    }
}

/// A node with no desktop of its own and a client peer granted `grant`, linked; the node serves helpers of `accounts`.
struct World {
    desktop: Arc<Painted>,
    host: Arc<Peer>,
    client: Arc<Peer>,
    link: PeerLink,
    endpoint: String,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    _wire: Wire,
    _dir: tempfile::TempDir,
}

impl World {
    fn new(accounts: &[String], grant: Grant, input_when_locked: bool) -> Self {
        let dir = tempfile::tempdir().expect("a directory");
        let endpoint = endpoint(&dir, "gui");
        let gui = host_config(accounts, &endpoint, input_when_locked);
        let host = Peer::new(gui.clone(), Some(Arc::new(Absent)));
        let client = Peer::new(GuiConfig::default(), None);
        host.keys.accept(client.id);
        host.grants.set(client.id, grant);
        client.keys.accept(host.id);
        let wire = Wire::make(&client, &host);
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        let listener = HelperListener::bind(&gui, &host.id)
            .expect("the helper endpoint is bound")
            .expect("helper accounts are listed");
        threads.push(thread::spawn({
            let (host, stop) = (host.clone(), stop.clone());
            move || {
                let _ = host.node.serve_helpers(&listener, &stop);
            }
        }));
        let until = Instant::now() + WAIT;
        let link = loop {
            if let Some(link) = client.node.peer(&host.id) {
                break link;
            }
            assert!(Instant::now() < until, "no link came up");
            thread::sleep(Duration::from_millis(20));
        };
        Self {
            desktop: Arc::new(Painted::new()),
            host,
            client,
            link,
            endpoint,
            stop,
            threads,
            _wire: wire,
            _dir: dir,
        }
    }

    /// Starts a helper over the world's painted desktop, trusting `node_account` as the node's, with the user's own `[gui]`.
    fn helper(
        &mut self,
        node_account: &str,
        user: GuiConfig,
    ) -> (Arc<AtomicBool>, JoinHandle<Result<(), String>>) {
        let settings = helper_settings(&self.endpoint, node_account, user);
        let sources = sources_of(&self.desktop);
        let stop = Arc::new(AtomicBool::new(false));
        let handle = thread::spawn({
            let stop = stop.clone();
            move || {
                bohio::gui::helper::run(&settings, sources, &stop)
                    .map_err(|error| error.to_string())
            }
        });
        (stop, handle)
    }

    /// Opens a stream, waiting for the helper to be listed.
    fn open(&self, want: &[Want]) -> Result<Gui, Error> {
        let until = Instant::now() + WAIT;
        loop {
            match Gui::open(&self.link, &GuiRequest::new(want.iter().copied())) {
                Ok(gui) => return Ok(gui),
                Err(error) if Instant::now() >= until => return Err(error),
                Err(_) => thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
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

fn remote(error: &Error) -> &ErrorBody {
    match error {
        Error::Remote(body) => body,
        other => panic!("expected the host's error, got {other:?}"),
    }
}

fn stop_helper(stop: &AtomicBool, handle: JoinHandle<Result<(), String>>) {
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

#[test]
fn a_helper_over_a_real_pipe_serves_a_snapshot_and_input_to_a_peer() {
    let mut world = World::new(&[me()], all(), false);
    let (stop, helper) = world.helper(&me(), GuiConfig::default());
    let gui = world
        .open(&[Want::View, Want::Input])
        .expect("the helper's session opens");
    // The session is named by the verified account, then the helper's own name for it.
    assert_eq!(gui.session(), format!("{}:main", me().to_ascii_uppercase()));
    // `info` lists the helper's session to a peer that may look, with the user the helper named.
    let info = Control::open(&world.link)
        .expect("a control stream")
        .info(Some(WAIT))
        .expect("info");
    assert!(info.gui.available, "{:?}", info.gui.reason);
    assert_eq!(info.gui.sessions.len(), 1);
    assert_eq!(info.gui.sessions[0].id, gui.session());
    assert_eq!(info.gui.sessions[0].displays.len(), 2);
    let image = gui
        .snapshot(&SnapshotOptions::default(), Some(WAIT))
        .expect("a snapshot through the helper");
    assert_eq!(image.header.width, 320);
    let click = InputEvent::Button {
        button: Button::Left,
        action: ButtonAction::Click,
        count: None,
        display: None,
        x: Some(100),
        y: Some(50),
    };
    assert_eq!(
        gui.input(&[click], Some(WAIT))
            .expect("input through the helper"),
        1
    );
    let (_, _, pixels) = world.desktop.pixels(0);
    let at = (50 * 320 + 100) * 3;
    assert_eq!(
        pixels[at..at + 3],
        CLICK,
        "the click painted where it landed"
    );
    // The node heard what the helper did, as counts.
    let until = Instant::now() + WAIT;
    gui.close();
    loop {
        let events = world.host.events.gui();
        if events.iter().any(|change| {
            matches!(
                change,
                GuiChange::Ended {
                    snapshots: 1,
                    input_events: 1,
                    ..
                }
            )
        }) {
            break;
        }
        assert!(
            Instant::now() < until,
            "the node never heard the stream end: {events:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    stop_helper(&stop, helper);
}

#[test]
fn a_connection_from_an_account_that_is_not_listed_is_closed_unread() {
    // Only another account is listed, so this account's helper is not served: no session, and the node says why.
    let mut world = World::new(&[someone_else()], all(), false);
    let (stop, helper) = world.helper(&me(), GuiConfig::default());
    let error = world
        .open(&[Want::View])
        .map(|_| ())
        .expect_err("a helper of an unlisted account was served");
    assert!(
        matches!(
            remote(&error).code,
            ErrorCode::Unavailable | ErrorCode::Unsupported
        ),
        "{error:?}"
    );
    let until = Instant::now() + WAIT;
    loop {
        let events = world.host.events.gui();
        if events.iter().any(|change| {
            matches!(change, GuiChange::Refused { reason } if reason.contains("not listed"))
        }) {
            break;
        }
        assert!(
            Instant::now() < until,
            "the refusal was not reported: {events:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    stop_helper(&stop, helper);
}

#[test]
fn a_listed_account_alone_is_served_among_several() {
    let mut world = World::new(&[someone_else(), me()], all(), false);
    let (stop, helper) = world.helper(&me(), GuiConfig::default());
    assert!(world.open(&[Want::View]).is_ok());
    stop_helper(&stop, helper);
}

#[test]
fn a_helper_refuses_a_node_of_another_account_before_it_sends_anything() {
    let mut world = World::new(&[me()], all(), false);
    // The endpoint is served by this account; the helper was told to trust another.
    let (stop, helper) = world.helper(&someone_else(), GuiConfig::default());
    let until = Instant::now() + WAIT;
    while !helper.is_finished() {
        if Instant::now() >= until {
            stop.store(true, Ordering::SeqCst);
            let _ = helper.join();
            panic!("a helper served a node of the wrong account");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let ended = helper.join().expect("the helper returns");
    let message = ended.expect_err("a helper served a node of the wrong account");
    assert!(
        message.contains(&me()),
        "it names who serves the endpoint: {message}"
    );
    assert!(
        message.contains(&someone_else()),
        "it names who it was told to trust: {message}"
    );
    // Nothing was sent: the node never saw a helper.
    let error = world
        .client_open_once(&[Want::View])
        .expect_err("a session was listed");
    assert!(
        matches!(
            remote(&error).code,
            ErrorCode::Unavailable | ErrorCode::Unsupported
        ),
        "{error:?}"
    );
}

impl World {
    fn client_open_once(&self, want: &[Want]) -> Result<Gui, Error> {
        thread::sleep(Duration::from_millis(500));
        Gui::open(&self.link, &GuiRequest::new(want.iter().copied()))
    }
}

#[test]
fn the_helper_command_exits_1_naming_both_accounts_when_another_serves_its_endpoint() {
    let dir = tempfile::tempdir().expect("a directory");
    let endpoint = endpoint(&dir, "wrong");
    let Endpoint::Pipe(name) = Endpoint::parse(&endpoint).expect("an endpoint") else {
        panic!("a pipe");
    };
    // A server of this account on the helper endpoint that reads whatever arrives: the command must never get to send.
    let listener = coqui_transport_pipe::Listener::builder()
        .name(&name)
        .build()
        .expect("a listener");
    let heard = Arc::new(AtomicBool::new(false));
    let serving = thread::spawn({
        let heard = heard.clone();
        move || {
            use std::io::Read as _;
            if let Ok(mut connection) = listener.accept_timeout(Some(Duration::from_secs(30))) {
                let mut byte = [0_u8; 1];
                if matches!(connection.read(&mut byte), Ok(1)) {
                    heard.store(true, Ordering::SeqCst);
                }
            }
        }
    });
    let config = dir.path().join("empty.cfg");
    std::fs::write(&config, "# nothing\n").expect("a config");
    let wrong = someone_else();
    let finished = support::procs::run(&[
        "gui-helper",
        "--config",
        &config.to_string_lossy(),
        "--endpoint",
        &endpoint,
        "--node-account",
        &wrong,
    ]);
    assert_eq!(finished.code, Some(1), "{}", finished.stderr);
    assert!(finished.stderr.contains(&me()), "{}", finished.stderr);
    assert!(finished.stderr.contains(&wrong), "{}", finished.stderr);
    let _ = serving.join();
    assert!(
        !heard.load(Ordering::SeqCst),
        "the helper sent to a server that is not its node's"
    );
}

#[test]
fn removing_gui_input_from_the_peer_ends_the_relayed_stream_and_refuses_the_next_frame() {
    let mut world = World::new(&[me()], all(), false);
    let (stop, helper) = world.helper(&me(), GuiConfig::default());
    let gui = world.open(&[Want::View, Want::Input]).expect("opens");
    gui.input(&[InputEvent::Text { text: "a".into() }], Some(WAIT))
        .expect("typed");
    world
        .host
        .grants
        .set(world.client.id, Grant::allowing([Permission::GuiView]));
    let late = gui
        .input(&[InputEvent::Text { text: "b".into() }], Some(WAIT))
        .expect_err("input after the revoke");
    assert_eq!(remote(&late).code, ErrorCode::Denied);
    assert_eq!(
        world.desktop.log(),
        ["text:a"],
        "nothing reached the desktop after the revoke"
    );
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
    stop_helper(&stop, helper);
}

#[test]
fn the_helpers_own_input_when_locked_false_refuses_what_the_node_allows() {
    // The node says input may go to a locked screen; the user of the helper does not.
    let mut world = World::new(&[me()], all(), true);
    world.desktop.set_locked(Some(true));
    let user = GuiConfig {
        input_when_locked: false,
        ..GuiConfig::default()
    };
    let (stop, helper) = world.helper(&me(), user);
    let gui = world.open(&[Want::View, Want::Input]).expect("opens");
    let refused = gui
        .input(&[InputEvent::Text { text: "x".into() }], Some(WAIT))
        .expect_err("typed into a locked screen");
    assert_eq!(remote(&refused).code, ErrorCode::Unavailable);
    assert!(world.desktop.log().is_empty());
    stop_helper(&stop, helper);
    // The same node and screen, with a user who allows it: the line above is the helper's doing, not the node's.
    let mut open = World::new(&[me()], all(), true);
    open.desktop.set_locked(Some(true));
    let permissive = GuiConfig {
        input_when_locked: true,
        ..GuiConfig::default()
    };
    let (stop, helper) = open.helper(&me(), permissive);
    let gui = open.open(&[Want::View, Want::Input]).expect("opens");
    assert_eq!(
        gui.input(&[InputEvent::Text { text: "x".into() }], Some(WAIT))
            .expect("typed"),
        1
    );
    stop_helper(&stop, helper);
}

#[test]
fn a_helper_that_ends_ends_its_streams_as_unavailable() {
    let mut world = World::new(&[me()], all(), false);
    let (stop, helper) = world.helper(&me(), GuiConfig::default());
    let gui = world.open(&[Want::View]).expect("opens");
    gui.snapshot(&SnapshotOptions::default(), Some(WAIT))
        .expect("one snapshot");
    stop_helper(&stop, helper);
    // Read without sending: a request sent after the stream ended would meet the ended stream before the node's last frame.
    let started = Instant::now();
    let ended = loop {
        match gui.next_event(Some(Duration::from_millis(300))) {
            Err(Error::Timeout) | Ok(_) => {}
            Err(error) => break error,
        }
        assert!(started.elapsed() < WAIT, "the stream outlived its helper");
    };
    match &ended {
        Error::Remote(body) => assert_eq!(body.code, ErrorCode::Unavailable, "{body:?}"),
        other => panic!("expected the session to end as unavailable, got {other:?}"),
    }
}

#[test]
fn a_peer_without_gui_permissions_is_denied_before_the_helper_is_asked() {
    let mut world = World::new(&[me()], Grant::allowing([Permission::Info]), false);
    let (stop, helper) = world.helper(&me(), GuiConfig::default());
    thread::sleep(Duration::from_secs(1));
    let error = Gui::open(&world.link, &GuiRequest::new([Want::View]))
        .map(|_| ())
        .expect_err("a peer without gui.view was served");
    assert!(
        matches!(error, Error::Remote(_) | Error::Transport(_)),
        "{error:?}"
    );
    assert!(world.desktop.log().is_empty(), "the desktop was reached");
    stop_helper(&stop, helper);
}

#[test]
fn two_helpers_of_one_session_leave_the_newer_one() {
    let mut world = World::new(&[me()], all(), false);
    let (stop_old, old) = world.helper(&me(), GuiConfig::default());
    let _ = world.open(&[Want::View]).expect("the first helper serves");
    let (stop_new, new) = world.helper(&me(), GuiConfig::default());
    let until = Instant::now() + WAIT;
    // The older helper's connection ends: its work is over, and it returns.
    let waited = thread::spawn(move || old.join());
    while !waited.is_finished() {
        assert!(Instant::now() < until, "the replaced helper kept running");
        thread::sleep(Duration::from_millis(50));
    }
    assert!(world.open(&[Want::View]).is_ok(), "the newer helper serves");
    stop_helper(&stop_new, new);
    let _ = stop_old;
}

#[test]
fn the_accounts_of_a_name_resolve_and_a_bad_one_is_refused_at_bind() {
    assert!(resolve_account(&me()).is_ok());
    let gui = GuiConfig {
        enabled: true,
        helper_accounts: vec!["no-such-account-bohio".to_owned()],
        helper_endpoint: Some("pipe://bohio-test-bad".to_owned()),
        ..GuiConfig::default()
    };
    let id = Identity::generate(Method::DEFAULT)
        .expect("an identity")
        .peer_id();
    assert!(HelperListener::bind(&gui, &id).is_err());
}

#[test]
fn a_peer_that_may_not_look_is_not_told_which_sessions_a_helper_serves() {
    let mut world = World::new(&[me()], all(), false);
    let (stop, helper) = world.helper(&me(), GuiConfig::default());
    // Once the helper is listed (a peer that may look can open it), the peer loses the right to look.
    let gui = world.open(&[Want::View]).expect("opens");
    gui.close();
    world
        .host
        .grants
        .set(world.client.id, Grant::allowing([Permission::Info]));
    let info = Control::open(&world.link)
        .expect("a control stream")
        .info(Some(WAIT))
        .expect("info");
    assert!(info.gui.sessions.is_empty(), "{:?}", info.gui.sessions);
    stop_helper(&stop, helper);
}

/// Serves a helper of the exec source `name` to the node of `world`, over a memory pipe: the account the node lists it under is
/// `exec:NAME`, as when the node starts the helper through that source.
fn exec_helper(world: &World, name: &str) -> (Arc<AtomicBool>, Vec<JoinHandle<()>>) {
    let settings = Settings::default();
    let (one, two) = memory::pipe(1024 * 1024);
    let node_end = Arc::new(stream(one, &settings).expect("a channel"));
    let helper_end = Arc::new(stream(two, &settings).expect("a channel"));
    let stop = Arc::new(AtomicBool::new(false));
    let helper = helper_settings(&world.endpoint, &me(), GuiConfig::default());
    let sources = sources_of(&world.desktop);
    let threads = vec![
        thread::spawn({
            let (host, stop, end, account) = (
                world.host.clone(),
                stop.clone(),
                node_end.clone(),
                format!("exec:{name}"),
            );
            move || {
                host.node.serve_helper_channel(&*end, &account, Some(&stop));
                end.close();
            }
        }),
        thread::spawn({
            let (stop, end) = (stop.clone(), helper_end.clone());
            move || {
                bohio::gui::helper::serve_node(&*end, &helper, sources, &stop);
                end.close();
            }
        }),
    ];
    (stop, threads)
}

#[test]
fn a_peer_reaches_the_helper_of_an_exec_source_only_with_that_source() {
    let world = World::new(&[me()], Grant::none(), false);
    let (stop, threads) = exec_helper(&world, "build");
    let looking = Grant::allowing([Permission::Info, Permission::GuiView, Permission::GuiInput]);
    let session = "exec:build:main";

    // Positive control: with the source named, the session is listed and opens (and the helper is surely up by then).
    world
        .host
        .grants
        .set(world.client.id, looking.clone().with_sources(["build"]));
    let gui = world.open(&[Want::View]).expect("the named source opens");
    assert_eq!(gui.session(), session);
    let info = Control::open(&world.link)
        .expect("a control stream")
        .info(Some(WAIT))
        .expect("info");
    assert_eq!(info.gui.sessions.len(), 1);

    // Without the source: not listed, not opened, by name or by default.
    world.host.grants.set(world.client.id, looking.clone());
    let info = Control::open(&world.link)
        .expect("a control stream")
        .info(Some(WAIT))
        .expect("info");
    assert!(info.gui.sessions.is_empty(), "{:?}", info.gui.sessions);
    for request in [
        GuiRequest::new([Want::View]),
        GuiRequest {
            session: Some(session.to_owned()),
            ..GuiRequest::new([Want::View])
        },
    ] {
        let error = Gui::open(&world.link, &request).expect_err("the source is not the peer's");
        // The link refuses a named session outright; a request that names none finds no desktop.
        assert!(
            matches!(error, Error::Transport(_))
                || matches!(
                    &error,
                    Error::Remote(body)
                        if matches!(body.code, ErrorCode::NotFound | ErrorCode::Unavailable)
                ),
            "{error:?}"
        );
    }

    // Narrowing the grant ends the stream that was open.
    world
        .host
        .grants
        .set(world.client.id, looking.with_sources(["build"]));
    let open = world.open(&[Want::View]).expect("opens again");
    world.host.grants.set(
        world.client.id,
        Grant::allowing([Permission::Info, Permission::GuiView]),
    );
    let until = Instant::now() + WAIT;
    while open
        .snapshot(&SnapshotOptions::default(), Some(Duration::from_secs(2)))
        .is_ok()
    {
        assert!(Instant::now() < until, "the stream outlived its source");
        thread::sleep(RECHECK);
    }
    stop.store(true, Ordering::SeqCst);
    for handle in threads {
        let _ = handle.join();
    }
}
