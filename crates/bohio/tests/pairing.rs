//! Pairing on a node's own listeners: a PIN armed on a running node is tried over tcp, ws, webtransport and pipe by the
//! listener that also serves proven peers, an unarmed node refuses, a wrong PIN uses the PIN up, an expired one is refused,
//! and the PIN is in no log or event.

#![forbid(unsafe_code)]

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{ControlConfig, LinkConfig, ListenConfig, NodeConfig, PairingConfig, Span};
use bohio::link::{Keys, NodeEvent, NodeEvents};
use bohio::listen::Listening;
use bohio::local::{Local, LocalLink};
use bohio::node::{Endpoint, MemoryGrants, MemoryKeys, Node, PairEnd};
use bohio::policy::{Grant, Permission};
use bohio_client::connect::Connection;
use bohio_client::control::Control;
use bohio_client::dial::ServerTrust;
use bohio_client::link::Settings;
use bohio_proto::local::PairReply;
use cabuya::auth::{Identity, Method, Proof, Trust};
use cabuya::pairing;
use coqui_peer::pair::{CodeAlphabet, Pin};
use coqui_peer::{PeerId, TrustStore};
use tempfile::TempDir;

const LIMIT: Duration = Duration::from_secs(30);

/// Every log line written in this test binary, at every level, with no timestamp.
static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Sink;

impl io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        LOG.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn capture_logs() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .without_time()
            .with_ansi(false)
            .with_writer(|| Sink)
            .try_init();
    });
}

/// Whether `text` holds `word` as a whole token: ids and ports never match a PIN by being inside a longer run.
fn holds_token(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token == word)
}

fn log_text() -> String {
    String::from_utf8_lossy(&LOG.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
}

#[derive(Default)]
struct Events(Mutex<Vec<NodeEvent>>);

impl NodeEvents for Events {
    fn event(&self, event: &NodeEvent) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event.clone());
    }
}

impl Events {
    fn all(&self) -> Vec<NodeEvent> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Where a listener is, and how a client trusts its certificate.
struct Where {
    scheme: &'static str,
    endpoint: Endpoint,
    trust: ServerTrust,
}

struct Host {
    node: Arc<Node>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    listeners: Vec<Where>,
    keys: Arc<MemoryKeys>,
    events: Arc<Events>,
    settings: Settings,
    known: Identity,
    _dir: TempDir,
}

fn settings() -> Settings {
    LinkConfig {
        proof_timeout: Span(Duration::from_secs(10)),
        ..LinkConfig::default()
    }
    .settings()
}

impl Host {
    /// A node listening on tcp, ws, webtransport and pipe, that can pair into a store of its own; `known` is a peer it
    /// accepts and lets read `info`.
    fn start(pairing: bool) -> Self {
        Self::start_with(pairing, PairingConfig::default())
    }

    fn start_with(pairing: bool, offer: PairingConfig) -> Self {
        capture_logs();
        static NUMBER: AtomicU32 = AtomicU32::new(0);
        let dir = tempfile::tempdir().expect("a directory");
        let config = NodeConfig {
            link: LinkConfig {
                recheck: Span(Duration::from_millis(500)),
                proof_timeout: Span(Duration::from_secs(10)),
                ..LinkConfig::default()
            },
            pairing: offer,
            ..NodeConfig::default()
        };
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let known = Identity::generate(Method::DEFAULT).expect("an identity");
        let keys = Arc::new(MemoryKeys::new());
        keys.accept(known.peer_id());
        let grants = Arc::new(MemoryGrants::new());
        grants.set(known.peer_id(), Grant::allowing([Permission::Info]));
        let events = Arc::new(Events::default());
        let mut node = Node::new(config.clone(), identity, keys.clone(), grants)
            .expect("the node is built")
            .with_events(events.clone());
        if pairing {
            let store = TrustStore::open(dir.path()).expect("a store");
            node = node.with_pairing(store).expect("pairing is set up");
        }
        let node = Arc::new(node);
        let stop = Arc::new(AtomicBool::new(false));
        let number = NUMBER.fetch_add(1, Ordering::SeqCst);
        let pipe = format!("bohio-pairing-test-{}-{number}", std::process::id());
        let mut threads = Vec::new();
        let mut listeners = Vec::new();
        for (scheme, text) in [
            ("tcp", "tcp://127.0.0.1:0".to_owned()),
            ("ws", "ws://127.0.0.1:0".to_owned()),
            ("webtransport", "webtransport://127.0.0.1:0".to_owned()),
            ("pipe", format!("pipe://{pipe}")),
        ] {
            let entry = ListenConfig {
                endpoint: text,
                ..ListenConfig::default()
            };
            let (listening, bound) =
                Listening::bind(&entry, &config.link.settings()).expect("the listener binds");
            let mut trust = ServerTrust::default();
            if let Some(hash) = listening.certificate_hash() {
                trust.pin(&hash).expect("a certificate hash");
            }
            listeners.push(Where {
                scheme,
                endpoint: bound,
                trust,
            });
            threads.push(thread::spawn({
                let (node, stop) = (Arc::clone(&node), Arc::clone(&stop));
                move || {
                    let _ = node.serve_listening(&listening, &stop);
                }
            }));
        }
        Self {
            node,
            stop,
            threads,
            listeners,
            keys,
            events,
            settings: settings(),
            known,
            _dir: dir,
        }
    }

    /// A joiner's attempt at `listener` with `pin`: its error as text, or the host's id it learned.
    fn join(&self, listener: &Where, pin: &str) -> Result<(PeerId, Identity, TempDir), String> {
        let dir = tempfile::tempdir().expect("a directory");
        let store = TrustStore::open(dir.path()).expect("a store");
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let channel = listener
            .endpoint
            .dial_with(LIMIT, &self.settings, &listener.trust)
            .map_err(|error| format!("dial: {error}"))?;
        let pin = Pin::parse(&CodeAlphabet::new("0123456789", pin.len()), pin).expect("a PIN");
        let joined = pairing::join_shared(&channel, &identity, pin, &store, LIMIT);
        coqui_transport::Channel::close(&channel);
        let paired = joined.map_err(|error| error.to_string())?;
        let stored = store.accepted().expect("the accepted keys");
        assert_eq!(
            stored,
            [paired.peer.id()],
            "the joiner stored the host's key"
        );
        Ok((paired.peer.id(), identity, dir))
    }

    /// The known peer reads `info` over `listener`: a proven peer is served there.
    fn proven_peer_is_served(&self, listener: &Where) {
        let proof = Proof::builder(self.known.clone())
            .responders(Trust::pinned([self.node.host()]))
            .build()
            .expect("a proof");
        let connection = Connection::connect_with(
            &listener.endpoint,
            LIMIT,
            &proof,
            &self.settings,
            &listener.trust,
        )
        .unwrap_or_else(|error| {
            panic!(
                "{}: the proven peer did not connect: {error}",
                listener.scheme
            )
        });
        let control = Control::open(&connection).expect("the control stream");
        control
            .info(Some(LIMIT))
            .unwrap_or_else(|error| panic!("{}: info failed: {error}", listener.scheme));
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[test]
fn a_pin_pairs_over_every_listener_that_goes_on_serving_proven_peers() {
    let host = Host::start(true);
    let mut pins = Vec::new();
    for listener in &host.listeners {
        host.proven_peer_is_served(listener);
        let armed = host.node.arm_pairing(None).expect("a PIN is armed");
        pins.push(armed.pin().to_owned());
        let (learned, joiner, _dir) = host
            .join(listener, armed.pin())
            .unwrap_or_else(|why| panic!("{}: {why}", listener.scheme));
        assert_eq!(learned, host.node.host(), "{}", listener.scheme);
        assert_eq!(
            armed.wait(LIMIT),
            Some(PairEnd::Paired(joiner.peer_id())),
            "{}",
            listener.scheme
        );
        assert!(
            host.keys.is_accepted(&joiner.peer_id()),
            "{}: the keys recorded the joiner",
            listener.scheme
        );
        host.proven_peer_is_served(listener);
    }
    let events = format!("{:?}", host.events.all());
    assert!(
        host.events
            .all()
            .iter()
            .filter(|event| matches!(event, NodeEvent::Paired { .. }))
            .count()
            == host.listeners.len(),
        "{events}"
    );
    let log = log_text();
    for pin in &pins {
        assert!(!holds_token(&events, pin), "a PIN is in an event");
        assert!(!holds_token(&log, pin), "a PIN is in the log");
    }
}

#[test]
fn an_unarmed_node_refuses_a_joiner_at_once_and_says_so_as_an_event() {
    let host = Host::start(true);
    for listener in &host.listeners {
        let why = host
            .join(listener, "123456")
            .expect_err(&format!("{}: an unarmed node paired", listener.scheme));
        assert!(
            why.contains("none is shown") || why.contains("not offered"),
            "{}: {why}",
            listener.scheme
        );
    }
    let refused = host
        .events
        .all()
        .iter()
        .filter(|event| matches!(event, NodeEvent::PairingRefused { .. }))
        .count();
    assert_eq!(refused, host.listeners.len());
    assert!(host.keys.is_accepted(&host.known.peer_id()));
}

#[test]
fn a_node_with_no_store_refuses_every_joiner() {
    let host = Host::start(false);
    let listener = &host.listeners[0];
    assert!(host.join(listener, "123456").is_err());
    assert!(host.node.arm_pairing(None).is_err());
    host.proven_peer_is_served(listener);
}

fn wrong_of(pin: &str) -> &'static str {
    if pin.chars().all(|c| c == '0') {
        "111111"
    } else {
        "000000"
    }
}

fn quick(max_attempts: u32) -> PairingConfig {
    PairingConfig {
        max_attempts,
        attempt_delay: Span(Duration::ZERO),
        attempt_delay_max: Span(Duration::ZERO),
        ..PairingConfig::default()
    }
}

#[test]
fn a_wrong_pin_does_not_use_the_pin_up_until_the_last_allowed_attempt_does() {
    let host = Host::start_with(true, quick(3));
    let listener = &host.listeners[0];
    let armed = host.node.arm_pairing(None).expect("a PIN is armed");
    let wrong = wrong_of(armed.pin());
    for _ in 0..2 {
        assert!(host.join(listener, wrong).is_err(), "a wrong PIN paired");
        assert_eq!(
            armed.wait(Duration::from_millis(200)),
            None,
            "used up early"
        );
    }
    assert!(host.join(listener, wrong).is_err());
    assert_eq!(armed.wait(LIMIT), Some(PairEnd::UsedUp));
    assert!(
        host.join(listener, armed.pin()).is_err(),
        "the right PIN paired after the last allowed guess"
    );
    let counts: Vec<u32> = host
        .events
        .all()
        .iter()
        .filter_map(|event| match event {
            NodeEvent::PairingRefused { attempts, .. } => Some(*attempts),
            _ => None,
        })
        .collect();
    assert_eq!(&counts[..3], [1, 2, 3], "{counts:?}");
}

#[test]
fn a_wrong_pin_then_the_right_one_pairs_when_attempts_remain() {
    let host = Host::start_with(true, quick(10));
    let listener = &host.listeners[0];
    let armed = host.node.arm_pairing(None).expect("a PIN is armed");
    assert!(host.join(listener, wrong_of(armed.pin())).is_err());
    let (_, joiner, _dir) = host
        .join(listener, armed.pin())
        .expect("the right PIN pairs");
    assert_eq!(armed.wait(LIMIT), Some(PairEnd::Paired(joiner.peer_id())));
}

#[test]
fn no_limit_keeps_the_pin_until_its_lifetime() {
    let host = Host::start_with(true, quick(0));
    let listener = &host.listeners[0];
    let armed = host.node.arm_pairing(None).expect("a PIN is armed");
    for _ in 0..12 {
        assert!(host.join(listener, wrong_of(armed.pin())).is_err());
    }
    assert_eq!(armed.wait(Duration::from_millis(200)), None);
    host.join(listener, armed.pin())
        .expect("the right PIN pairs");
}

/// Whether the most recent refusal the host reported was "too soon": the source was still within its wait, nothing was tried.
fn last_refusal_was_too_soon(host: &Host) -> bool {
    host.events
        .all()
        .iter()
        .rev()
        .find_map(|event| match event {
            NodeEvent::PairingRefused { reason, .. } => Some(reason.starts_with("too soon")),
            _ => None,
        })
        .unwrap_or(false)
}

/// Guesses wrong until the host takes the guess (a refusal that is not "too soon" is the host having tried it), asking again
/// every `poll`; a guess made within the source's wait is refused untried and counts for nothing. Returns when the host took it.
fn guess_when_allowed(host: &Host, listener: &Where, wrong: &str, poll: Duration) -> Instant {
    loop {
        let before = host.events.all().len();
        assert!(host.join(listener, wrong).is_err());
        let ended = Instant::now();
        if host.events.all().len() > before && !last_refusal_was_too_soon(host) {
            return ended;
        }
        thread::sleep(poll);
    }
}

#[test]
fn a_source_that_guessed_wrong_waits_and_the_wait_grows() {
    // The waits are measured, never slept against: each is the time from one taken guess to the next, which can only be
    // longer than the host's wait (a late machine makes it longer, not shorter). The slack for the host's clock starting a
    // moment after the joiner saw its refusal is a quarter of the configured delay.
    let delay = Duration::from_millis(600);
    let max = Duration::from_millis(1200);
    let slack = delay / 4;
    let settings = PairingConfig {
        attempt_delay: Span(delay),
        attempt_delay_max: Span(max),
        ..PairingConfig::default()
    };
    let host = Host::start_with(true, settings);
    let listener = &host.listeners[0];
    let armed = host.node.arm_pairing(None).expect("a PIN is armed");
    let wrong = wrong_of(armed.pin());
    assert!(host.join(listener, wrong).is_err());
    let first = Instant::now();
    // Too soon: refused without a try, and it does not count as a guess.
    assert!(host.join(listener, wrong).is_err());
    assert!(
        first.elapsed() < delay - slack,
        "the machine stalled for most of a wait: the test cannot tell too soon from late"
    );
    let reasons: Vec<String> = host
        .events
        .all()
        .iter()
        .filter_map(|event| match event {
            NodeEvent::PairingRefused {
                reason, attempts, ..
            } => Some(format!("{attempts} {reason}")),
            _ => None,
        })
        .collect();
    assert!(reasons[1].starts_with("0 too soon"), "{reasons:?}");

    let poll = delay / 12;
    let second = guess_when_allowed(&host, listener, wrong, poll);
    assert!(
        second.duration_since(first) >= delay - slack,
        "the first wait was {:?}, not {delay:?}",
        second.duration_since(first)
    );
    let third = guess_when_allowed(&host, listener, wrong, poll);
    assert!(
        third.duration_since(second) >= delay * 2 - slack,
        "the wait did not grow: {:?} after the second guess",
        third.duration_since(second)
    );
    // The right PIN pairs once the wait is over, and is refused untried before it.
    let paired = (0..200).find_map(|_| match host.join(listener, armed.pin()) {
        Ok(joined) => Some(joined),
        Err(_) => {
            thread::sleep(poll);
            None
        }
    });
    assert!(
        paired.is_some(),
        "the right PIN never paired after the wait"
    );
    assert!(
        third.elapsed() >= max - slack,
        "it paired {:?} after the third guess, inside the wait capped at {max:?}",
        third.elapsed()
    );
}

#[test]
fn a_longer_pin_pairs() {
    let settings = PairingConfig {
        pin_length: 9,
        ..quick(10)
    };
    let host = Host::start_with(true, settings);
    let armed = host.node.arm_pairing(None).expect("a PIN is armed");
    assert_eq!(armed.pin().len(), 9);
    host.join(&host.listeners[1], armed.pin())
        .expect("it pairs");
}

#[test]
fn an_expired_pin_is_refused() {
    let host = Host::start(true);
    let listener = &host.listeners[0];
    let armed = host
        .node
        .arm_pairing(Some(Duration::from_millis(300)))
        .expect("a PIN is armed");
    thread::sleep(Duration::from_millis(600));
    assert!(
        host.join(listener, armed.pin()).is_err(),
        "an expired PIN paired"
    );
    assert_eq!(armed.wait(LIMIT), Some(PairEnd::Expired));
}

#[test]
fn arming_again_replaces_the_pin_and_a_withdrawn_pin_pairs_nobody() {
    let host = Host::start(true);
    let listener = &host.listeners[0];
    let first = host.node.arm_pairing(None).expect("a PIN is armed");
    let second = host.node.arm_pairing(None).expect("a PIN is armed");
    assert_eq!(first.wait(LIMIT), Some(PairEnd::Replaced));
    second.withdraw();
    assert_eq!(second.wait(LIMIT), Some(PairEnd::Withdrawn));
    assert!(host.join(listener, second.pin()).is_err());
    assert!(host.node.arm_pairing(Some(Duration::ZERO)).is_err());
}

#[test]
fn the_local_endpoint_arms_a_pin_and_reports_who_paired() {
    let host = Host::start(true);
    let name = format!("pipe://bohio-pairing-control-{}", std::process::id());
    let control = ControlConfig {
        endpoint: Some(name),
        ..ControlConfig::default()
    };
    let local = Local::bind(&control, &host.node.host()).expect("the control endpoint binds");
    let endpoint = local.endpoint().clone();
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let serving = scope.spawn(|| host.node.serve_local(&local, &stop));
        let link = LocalLink::connect(&endpoint, &host.settings).expect("the control connects");
        let pairing = link.pair(None).expect("a PIN is armed");
        let pin = pairing.pin().to_owned();
        let (learned, joiner, _dir) = host.join(&host.listeners[1], &pin).expect("it pairs");
        assert_eq!(learned, host.node.host());
        assert_eq!(
            pairing.wait().expect("the node reports"),
            PairReply::Paired {
                peer: joiner.peer_id().to_string()
            }
        );
        // A client that goes away withdraws the PIN.
        let gone = link.pair(None).expect("a PIN is armed");
        let pin = gone.pin().to_owned();
        drop(gone);
        thread::sleep(Duration::from_millis(500));
        assert!(host.join(&host.listeners[0], &pin).is_err());
        drop(link);
        stop.store(true, Ordering::SeqCst);
        let _ = serving.join();
    });
}

#[test]
fn the_local_endpoint_of_a_node_with_no_store_says_so() {
    let host = Host::start(false);
    let name = format!("pipe://bohio-pairing-nostore-{}", std::process::id());
    let control = ControlConfig {
        endpoint: Some(name),
        ..ControlConfig::default()
    };
    let local = Local::bind(&control, &host.node.host()).expect("the control endpoint binds");
    let endpoint = local.endpoint().clone();
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let serving = scope.spawn(|| host.node.serve_local(&local, &stop));
        let link = LocalLink::connect(&endpoint, &host.settings).expect("the control connects");
        let error = link
            .pair(None)
            .expect_err("a node with no store armed a PIN");
        assert!(error.to_string().contains("no store"), "{error}");
        drop(link);
        stop.store(true, Ordering::SeqCst);
        let _ = serving.join();
    });
}

#[test]
fn a_joiner_that_goes_silent_after_the_marker_frees_its_place_at_the_exchange_timeout() {
    let settings = PairingConfig {
        exchange_timeout: Span(Duration::from_millis(400)),
        ..PairingConfig::default()
    };
    let host = Host::start_with(true, settings);
    let _armed = host.node.arm_pairing(None).expect("a PIN is armed");
    let listener = &host.listeners[0];
    let channel = listener
        .endpoint
        .dial_with(LIMIT, &host.settings, &listener.trust)
        .expect("the listener is reached");
    coqui_transport::Channel::send(&channel, pairing::MARKER).expect("the marker is sent");
    let began = std::time::Instant::now();
    // The host gives up on the silent joiner and closes: what it reads then is the end of the stream, not a timeout.
    let ended = coqui_transport::Channel::receive(&channel, Some(Duration::from_secs(5)));
    assert!(
        !matches!(ended, Ok(Some(_)) | Err(coqui_transport::Failure::Timeout)),
        "the host never closed on the silent joiner: {ended:?}"
    );
    // The proof timeout is 10 s: a close within 4 s is the exchange timeout's doing.
    assert!(
        began.elapsed() < Duration::from_secs(4),
        "{:?}",
        began.elapsed()
    );
    coqui_transport::Channel::close(&channel);
}
