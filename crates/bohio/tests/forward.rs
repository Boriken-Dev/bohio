//! Forwarding: `bohio-connect/1` and `tcp/1` against an embedded node, and `bohio remote ... connect` and `forward` through the
//! built binary, over loopback.

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{ForwardConfig, LinkConfig, ListenConfig, NodeConfig, Span};
use bohio::listen::Listening;
use bohio::node::{MemoryGrants, MemoryKeys, Node};
use bohio::policy::{Grant, Permission};
use bohio_client::Error;
use bohio_client::connect::Connection;
use bohio_client::forward::{ForwardSettings, Target, connect};
use bohio_client::link::Settings;
use bohio_proto::ErrorCode;
use coqui_link::auth::{Identity, Method, Proof, Trust};
use coqui_link::mux::LinkStream;
use coqui_peer::PeerId;
use coqui_transport::{Channel, Failure};

/// A wait long enough for any exchange on this machine, short enough to fail a hung test.
const WAIT: Duration = Duration::from_secs(20);

/// The node's re-check interval in these tests.
const RECHECK: Duration = Duration::from_millis(500);

/// What scheduling and a busy machine may add to "within `2 x recheck`".
const SLACK: Duration = Duration::from_secs(3);

/// A running node, stopped and joined when dropped.
struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    endpoint: String,
    address: SocketAddr,
    id: PeerId,
    keys: Arc<MemoryKeys>,
    grants: Arc<MemoryGrants>,
}

impl Running {
    fn start() -> Self {
        Self::with(ForwardConfig::default())
    }

    fn with(forward: ForwardConfig) -> Self {
        let config = NodeConfig {
            link: LinkConfig {
                recheck: Span(RECHECK),
                ..LinkConfig::default()
            },
            forward,
            ..NodeConfig::default()
        };
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let node = Arc::new(
            Node::new(config.clone(), identity, keys.clone(), grants.clone())
                .expect("the node is built")
                .with_path(std::env::var_os("PATH").unwrap_or_default()),
        );
        let entry = ListenConfig {
            endpoint: "tcp://127.0.0.1:0".to_owned(),
            ..ListenConfig::default()
        };
        let (listening, bound) =
            Listening::bind(&entry, &config.link.settings()).expect("the listener binds");
        let endpoint = bound.to_string();
        let address = endpoint
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
            stop,
            thread: Some(thread),
            endpoint,
            address,
            id,
            keys,
            grants,
        }
    }

    /// A library peer holding `grant`, linked to the node.
    fn link(&self, grant: Grant) -> (Connection, PeerId) {
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let peer = identity.peer_id();
        self.keys.accept(peer);
        self.grants.set(peer, grant);
        let proof = Proof::builder(identity)
            .responders(Trust::pinned([self.id]))
            .build()
            .expect("a proof");
        let link = Connection::connect_tcp(
            self.address,
            Duration::from_secs(10),
            &proof,
            &Settings::default(),
        )
        .expect("the link is up");
        (link, peer)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A grant that lets the peer reach `rules`.
fn granting(rules: &[&str]) -> Grant {
    Grant::allowing([Permission::ForwardConnect]).with_connect(rules.iter().copied())
}

/// A TCP target on loopback.
fn loopback(port: u16) -> Target {
    Target::Tcp {
        host: "127.0.0.1".to_owned(),
        port,
    }
}

/// A listener on a free loopback port, and the port.
fn listening() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    (listener, port)
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    listening().1
}

/// Serves one connection with `serve` on a thread of its own.
fn serve_once(
    listener: TcpListener,
    serve: impl FnOnce(TcpStream) + Send + 'static,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let (connection, _) = listener.accept().expect("a connection");
        serve(connection);
    })
}

/// Reads until the end of the stream, which has to be a `finish`.
fn read_all(stream: &dyn LinkStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        match stream.receive(Some(WAIT)) {
            Ok(Some(message)) => bytes.extend_from_slice(&message),
            Ok(None) => return bytes,
            Err(failure) => panic!("the stream failed: {failure:?}"),
        }
    }
}

fn permission_refused(error: &Error) -> bool {
    matches!(error, Error::Transport(Failure::Permission))
}

#[test]
fn a_connect_echoes_with_a_half_close() {
    let node = Running::start();
    let (listener, port) = listening();
    // The target answers only after it has read the end of what the client sent.
    let server = serve_once(listener, |mut connection| {
        let mut request = Vec::new();
        connection.read_to_end(&mut request).expect("a request");
        connection
            .write_all(&[b"reply to ", request.as_slice()].concat())
            .expect("a reply");
    });
    let rule = format!("127.0.0.1:{port}");
    let (link, _) = node.link(granting(&[rule.as_str()]));
    let tunnel = connect(&link, &loopback(port), &ForwardSettings::default()).expect("connected");
    assert_eq!(tunnel.address(), Some(format!("127.0.0.1:{port}").as_str()));
    let stream = tunnel.stream();
    stream.send(b"hello").expect("sent");
    stream.finish().expect("finished");
    assert_eq!(read_all(stream), b"reply to hello");
    assert!(
        stream.finished_by_peer(),
        "the target's end arrived as a finish, not as a cut"
    );
    server.join().expect("the target ended");
}

#[test]
fn a_target_the_grant_does_not_cover_is_refused_before_anything_connects() {
    let node = Running::start();
    let (listener, port) = listening();
    listener.set_nonblocking(true).expect("non-blocking");
    let rule = format!("127.0.0.1:{port}");
    let other = format!("127.0.0.1:{}", port ^ 1);
    let no_permission = Grant::allowing([Permission::Info]).with_connect([rule.as_str()]);
    let no_rule = Grant::allowing([Permission::ForwardConnect]);
    let other_port = granting(&[other.as_str()]);
    for (label, grant) in [
        ("a rule without the permission", no_permission),
        ("the permission without a rule", no_rule),
        ("a rule for another port", other_port),
    ] {
        let (link, _) = node.link(grant);
        let error = connect(&link, &loopback(port), &ForwardSettings::default()).expect_err(label);
        assert!(permission_refused(&error), "{label}: {error:?}");
    }
    thread::sleep(Duration::from_millis(300));
    assert!(
        listener.accept().is_err(),
        "the host connected to a target its grant does not cover"
    );
}

#[test]
fn a_closed_port_is_refused_with_its_reason() {
    let node = Running::start();
    let port = closed_port();
    let rule = format!("127.0.0.1:{port}");
    let (link, _) = node.link(granting(&[rule.as_str()]));
    let error = connect(&link, &loopback(port), &ForwardSettings::default())
        .expect_err("nothing listens there");
    assert!(
        matches!(&error, Error::Remote(body) if body.code == ErrorCode::Refused),
        "{error:?}"
    );
}

#[test]
fn a_name_that_does_not_resolve_is_not_found() {
    let node = Running::start();
    let (link, _) = node.link(granting(&["*.invalid:*"]));
    let target = Target::Tcp {
        host: "no-such-host.invalid".to_owned(),
        port: 80,
    };
    let error = connect(&link, &target, &ForwardSettings::default()).expect_err("no such name");
    assert!(
        matches!(&error, Error::Remote(body) if body.code == ErrorCode::NotFound),
        "{error:?}"
    );
}

#[test]
fn rules_match_networks_ranges_and_names_and_nothing_else() {
    let node = Running::start();
    let (listener, port) = listening();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    listener.set_nonblocking(true).expect("non-blocking");
    let stop = Arc::new(AtomicBool::new(false));
    let target = thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut connection, _)) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        let _ = connection.write_all(b"hi");
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
        }
    });
    let name = |host: &str| Target::Tcp {
        host: host.to_owned(),
        port,
    };
    let reaches = |rule: &str, target: &Target| {
        let (link, _) = node.link(granting(&[rule]));
        match connect(&link, target, &ForwardSettings::default()) {
            Ok(tunnel) => {
                assert_eq!(read_all(tunnel.stream())[..2], *b"hi");
                true
            }
            Err(error) if permission_refused(&error) => false,
            Err(error) => panic!("{rule}: {error:?}"),
        }
    };
    let (low, high) = (port.saturating_sub(1).max(1), port.saturating_add(1));
    // An exact address, a network, a range and any port.
    assert!(reaches(&format!("127.0.0.1:{port}"), &name("127.0.0.1")));
    assert!(reaches(&format!("127.0.0.0/8:{port}"), &name("127.0.0.1")));
    assert!(reaches(
        &format!("127.0.0.1:{low}-{high}"),
        &name("127.0.0.1")
    ));
    assert!(reaches("127.0.0.1:*", &name("127.0.0.1")));
    // A name is matched as a name and never resolved: `localhost` is not `127.0.0.1`, and no network rule covers a name.
    assert!(reaches(&format!("localhost:{port}"), &name("localhost")));
    assert!(reaches(&format!("*:{port}"), &name("localhost")));
    assert!(!reaches(&format!("127.0.0.0/8:{port}"), &name("localhost")));
    assert!(!reaches(&format!("localhost:{port}"), &name("127.0.0.1")));
    // The wrong network, the wrong port and the wrong range.
    assert!(!reaches(&format!("10.0.0.0/8:{port}"), &name("127.0.0.1")));
    assert!(!reaches(
        &format!("127.0.0.1:{}", port ^ 1),
        &name("127.0.0.1")
    ));
    assert!(!reaches(
        &format!(
            "127.0.0.1:{}-{}",
            port.saturating_add(1),
            port.saturating_add(9)
        ),
        &name("127.0.0.1")
    ));
    stop.store(true, Ordering::SeqCst);
    target.join().expect("the target ended");
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        6,
        "a connection was made for a rule that does not match, or one that matches was not made"
    );
}

#[cfg(unix)]
#[test]
fn a_unix_socket_target_echoes_with_a_half_close() {
    use coqui_transport_pipe::{Listener, PipeName};
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("echo.sock");
    let name = PipeName::at(&path).expect("a socket name");
    let listener = Listener::builder().name(&name).build().expect("it listens");
    let server = thread::spawn(move || {
        let mut connection = listener.accept().expect("a connection");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 256];
        loop {
            match connection.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
                Err(error) => panic!("{error}"),
            }
        }
        connection
            .write_all(&[b"socket got ", request.as_slice()].concat())
            .expect("a reply");
    });
    let node = Running::start();
    let rule = format!("unix:{}/*.sock", dir.path().display());
    let (link, _) = node.link(granting(&[rule.as_str()]));
    let target = Target::Unix {
        path: path.display().to_string(),
    };
    let tunnel = connect(&link, &target, &ForwardSettings::default()).expect("connected");
    assert_eq!(tunnel.address(), None);
    tunnel.stream().send(b"ping").expect("sent");
    tunnel.stream().finish().expect("finished");
    assert_eq!(read_all(tunnel.stream()), b"socket got ping");
    server.join().expect("the target ended");

    // A socket that does not exist, and a path the rule does not cover.
    let missing = Target::Unix {
        path: dir.path().join("none.sock").display().to_string(),
    };
    let error = connect(&link, &missing, &ForwardSettings::default()).expect_err("no socket");
    assert!(
        matches!(&error, Error::Remote(body) if body.code == ErrorCode::NotFound),
        "{error:?}"
    );
    let outside = Target::Unix {
        path: format!("{}/../etc.sock", dir.path().display()),
    };
    let error = connect(&link, &outside, &ForwardSettings::default()).expect_err("outside");
    assert!(permission_refused(&error), "{error:?}");
}

#[cfg(windows)]
#[test]
fn a_named_pipe_target_echoes() {
    use coqui_transport_pipe::{Listener, PipeName};
    let name = format!("bohio-forward-test-{}", std::process::id());
    let pipe = PipeName::at(format!(r"\\.\pipe\{name}")).expect("a pipe name");
    let listener = Listener::builder().name(&pipe).build().expect("it listens");
    // A pipe has no half-close on Windows: the target answers each message, and the client ends the stream.
    let server = thread::spawn(move || {
        let mut connection = listener.accept().expect("a connection");
        let mut buffer = [0_u8; 256];
        loop {
            match connection.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    connection
                        .write_all(&[b"pipe got ", &buffer[..read]].concat())
                        .expect("a reply");
                }
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
    });
    let node = Running::start();
    let rule = format!("pipe:{}", name.replace("test-", "*"));
    let (link, _) = node.link(granting(&[rule.as_str()]));
    let target = Target::Pipe { name: name.clone() };
    let tunnel = connect(&link, &target, &ForwardSettings::default()).expect("connected");
    tunnel.stream().send(b"ping").expect("sent");
    let reply = tunnel
        .stream()
        .receive(Some(WAIT))
        .expect("a reply")
        .expect("not the end");
    assert_eq!(reply, b"pipe got ping");
    drop(tunnel);
    server.join().expect("the target ended");

    let other = Target::Pipe {
        name: "bohio-other".to_owned(),
    };
    let error = connect(&link, &other, &ForwardSettings::default()).expect_err("no rule");
    assert!(permission_refused(&error), "{error:?}");
}

#[test]
fn a_revoke_ends_a_running_forward_within_two_rechecks_and_the_target_sees_a_reset() {
    let node = Running::start();
    let (listener, port) = listening();
    let (ended, outcome) = mpsc::channel();
    let server = serve_once(listener, move |mut connection| {
        // Nothing is ever sent: the read ends only when the host drops the connection.
        let mut buffer = [0_u8; 16];
        let _ = ended.send(connection.read(&mut buffer));
    });
    let rule = format!("127.0.0.1:{port}");
    let (link, peer) = node.link(granting(&[rule.as_str()]));
    let tunnel = connect(&link, &loopback(port), &ForwardSettings::default()).expect("connected");
    node.grants.remove(&peer);
    let started = Instant::now();
    let failure = loop {
        match tunnel.stream().receive(Some(Duration::from_millis(100))) {
            Err(Failure::Timeout) | Ok(Some(_)) => {}
            Err(failure) => break failure,
            Ok(None) => panic!("the stream ended in order, not as a revoke"),
        }
        assert!(started.elapsed() < WAIT, "the forward was not ended");
    };
    assert_eq!(
        failure,
        Failure::Permission,
        "the peer was not told it was revoked"
    );
    assert!(
        started.elapsed() <= RECHECK * 2 + SLACK,
        "took {:?}",
        started.elapsed()
    );
    let seen = outcome
        .recv_timeout(WAIT)
        .expect("the target heard the end");
    assert!(
        seen.is_err(),
        "the target saw a clean end, not a reset: {seen:?}"
    );
    server.join().expect("the target ended");
}

#[test]
fn a_coqui_link_peer_uses_the_nodes_tcp_1_with_the_same_grant() {
    use coqui_link::auth::{Sealed, prove_out};
    use coqui_link::forward::{Target as LinkTarget, connect as link_connect};
    use coqui_link::mux::{Mux, Side};

    let node = Running::start();
    let (listener, port) = listening();
    let server = serve_once(listener, |mut connection| {
        let mut request = Vec::new();
        connection.read_to_end(&mut request).expect("a request");
        connection
            .write_all(&[b"tcp/1 got ", request.as_slice()].concat())
            .expect("a reply");
    });
    let identity = Identity::generate(Method::DEFAULT).expect("an identity");
    let peer = identity.peer_id();
    node.keys.accept(peer);
    let rule = format!("127.0.0.1:{port}");
    node.grants.set(peer, granting(&[rule.as_str()]));
    let proof = Proof::builder(identity)
        .responders(Trust::pinned([node.id]))
        .build()
        .expect("a proof");
    let settings = Settings::default();
    let socket = TcpStream::connect(node.address).expect("connected");
    let channel = bohio_client::link::socket(socket, &settings).expect("a channel");
    let established = prove_out(
        &proof,
        &channel,
        settings.proof_timeout,
        settings.record_length(proof.method()),
    )
    .expect("proven");
    let _ = coqui_transport::Channel::set_pending(&channel, settings.read_ahead);
    let sealed = Sealed::new(&channel, established.session);
    Mux::run(&sealed, Side::Dialed, settings.limits(), None, |mux| {
        let target: LinkTarget = format!("127.0.0.1:{port}").parse().expect("a target");
        let stream = link_connect(mux, &target).expect("opened");
        stream.send(b"from coqui-link").expect("sent");
        stream.finish().expect("finished");
        assert_eq!(read_all(&stream), b"tcp/1 got from coqui-link");

        // A target the grant does not cover is a refusal on the stream, before anything connects.
        let other: LinkTarget = "127.0.0.1:1".parse().expect("a target");
        let refused = link_connect(mux, &other).expect("opened");
        assert_eq!(refused.receive(Some(WAIT)), Err(Failure::Permission));
    });
    channel.close();
    server.join().expect("the target ended");
}

#[test]
fn the_bounds_are_settings_one_past_them_is_busy_and_forwarding_can_be_off() {
    let node = Running::with(ForwardConfig {
        max_connections_per_peer: 1,
        ..ForwardConfig::default()
    });
    let (listener, port) = listening();
    let server = thread::spawn(move || {
        let mut held = Vec::new();
        while let Ok((connection, _)) = listener.accept() {
            held.push(connection);
            if held.len() == 1 {
                break;
            }
        }
        thread::sleep(Duration::from_secs(1));
    });
    let rule = format!("127.0.0.1:{port}");
    let (link, _) = node.link(granting(&[rule.as_str()]));
    let first = connect(&link, &loopback(port), &ForwardSettings::default()).expect("the first");
    let error = connect(&link, &loopback(port), &ForwardSettings::default()).expect_err("one past");
    assert!(
        matches!(&error, Error::Remote(body) if body.code == ErrorCode::Busy),
        "{error:?}"
    );
    drop(first);
    drop(link);
    server.join().expect("the target ended");

    let node = Running::with(ForwardConfig {
        enabled: false,
        ..ForwardConfig::default()
    });
    let (link, _) = node.link(granting(&[rule.as_str()]));
    let error = connect(&link, &loopback(port), &ForwardSettings::default()).expect_err("off");
    assert!(
        matches!(&error, Error::Remote(body) if body.code == ErrorCode::Unsupported),
        "{error:?}"
    );
}

/// What the library peer's `info` says of forwarding.
#[test]
fn info_says_what_the_host_connects_to() {
    use bohio_client::control::Control;
    let node = Running::start();
    let (link, _) = node.link(Grant::allowing([Permission::Info]));
    let control = Control::open(&link).expect("a control stream");
    let info = control.info(Some(WAIT)).expect("info");
    assert!(info.forward.enabled);
    assert!(info.forward.connect.contains(&"tcp".to_owned()));
    assert_eq!(
        info.forward.connect.contains(&"unix".to_owned()),
        cfg!(unix)
    );
    assert_eq!(
        info.forward.connect.contains(&"pipe".to_owned()),
        cfg!(windows)
    );
}

// The command line.

fn client_config(dir: &Path, name: &str) -> PathBuf {
    let config = dir.join(format!("{name}.cfg"));
    std::fs::write(
        &config,
        format!(
            "[service]\nname = \"{name}\"\ndata_dir = '{}'\n",
            dir.join(format!("{name}-data")).display()
        ),
    )
    .expect("the configuration is written");
    config
}

fn bohio(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bohio"));
    command
        .args(args)
        .env_remove("BOHIO_CONFIG")
        .env("BOHIO_LOG", "warn")
        .env("NO_COLOR", "1");
    command
}

fn id_of(config: &Path) -> PeerId {
    let output = bohio(&["id", "--config", &config.to_string_lossy()])
        .output()
        .expect("bohio runs");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("a peer id")
}

/// A child that is killed when it drops, with its standard output and error read on threads.
struct Spawned {
    child: Child,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

fn collect(mut source: impl Read + Send + 'static) -> Arc<Mutex<Vec<u8>>> {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&bytes);
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(count) = source.read(&mut buffer) {
            if count == 0 {
                break;
            }
            shared.lock().unwrap().extend_from_slice(&buffer[..count]);
        }
    });
    bytes
}

impl Spawned {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("bohio starts");
        let stdout = collect(child.stdout.take().unwrap());
        let stderr = collect(child.stderr.take().unwrap());
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn stdout(&self) -> Vec<u8> {
        self.stdout.lock().unwrap().clone()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }

    /// Waits for the child to end and for the rest of its output.
    fn finish(&mut self) -> Option<i32> {
        let until = Instant::now() + WAIT * 2;
        loop {
            if let Some(status) = self.child.try_wait().expect("the child can be asked") {
                thread::sleep(Duration::from_millis(200));
                return status.code();
            }
            assert!(
                Instant::now() < until,
                "bohio did not end in time\nstderr: {}",
                self.stderr()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The first standard-output line that starts with `prefix`, waiting for it.
    fn line(&self, prefix: &str) -> String {
        let until = Instant::now() + WAIT;
        loop {
            let text = String::from_utf8_lossy(&self.stdout()).into_owned();
            if let Some(rest) = text.lines().find_map(|line| line.strip_prefix(prefix)) {
                return rest.trim().to_owned();
            }
            assert!(
                Instant::now() < until,
                "no `{prefix}` line\nstdout: {text}\nstderr: {}",
                self.stderr()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The arguments of `bohio remote` to `node` as the peer whose configuration is `config`.
fn remote_args<'a>(node: &'a Running, config: &'a str, server: &'a str) -> Vec<&'a str> {
    vec![
        "remote",
        node.endpoint.as_str(),
        "--config",
        config,
        "--server",
        server,
    ]
}

/// A node that grants a command-line peer the targets `rules`; the node, the peer's configuration, the node's id and the
/// peer's id.
fn node_and_peer(dir: &Path, rules: &[&str]) -> (Running, String, String, PeerId) {
    let node = Running::start();
    let config = client_config(dir, "peer");
    let peer = id_of(&config);
    node.keys.accept(peer);
    node.grants.set(peer, granting(rules));
    let server = node.id.to_string();
    (node, config.to_string_lossy().into_owned(), server, peer)
}

#[test]
fn connect_bridges_standard_input_and_output_and_check_prints_the_address() {
    let dir = tempfile::tempdir().expect("a directory");
    let (listener, port) = listening();
    let rule = format!("127.0.0.1:{port}");
    let (node, config, server, peer) = node_and_peer(dir.path(), &[rule.as_str()]);
    let target = format!("127.0.0.1:{port}");
    let answered = serve_once(listener, |mut connection| {
        let mut request = Vec::new();
        connection.read_to_end(&mut request).expect("a request");
        connection
            .write_all(&[b"answer to ", request.as_slice()].concat())
            .expect("a reply");
    });

    let mut args = remote_args(&node, &config, &server);
    args.extend(["connect", target.as_str()]);
    let mut child = Spawned::start(bohio(&args));
    let mut stdin = child.child.stdin.take().expect("a pipe to stdin");
    stdin.write_all(b"ssh-like bytes").expect("written");
    // Closing standard input is the half-close; the answer still comes.
    drop(stdin);
    assert_eq!(child.finish(), Some(0), "{}", child.stderr());
    assert_eq!(child.stdout(), b"answer to ssh-like bytes");
    answered.join().expect("the target ended");

    let (listener, port) = listening();
    let rule = format!("127.0.0.1:{port}");
    node.grants.set(peer, granting(&[rule.as_str()]));
    let target = format!("127.0.0.1:{port}");
    let mut args = remote_args(&node, &config, &server);
    args.extend(["connect", "--check", target.as_str()]);
    let mut check = Spawned::start(bohio(&args));
    assert_eq!(check.finish(), Some(0), "{}", check.stderr());
    assert_eq!(
        String::from_utf8_lossy(&check.stdout()).trim(),
        format!("connected {target}")
    );
    drop(listener);

    // A target the grant does not cover is exit 1 with the reason, and nothing on standard output.
    let mut args = remote_args(&node, &config, &server);
    args.extend(["connect", "--check", "127.0.0.1:1"]);
    let mut denied = Spawned::start(bohio(&args));
    assert_eq!(denied.finish(), Some(1));
    assert!(denied.stdout().is_empty());
    assert!(
        denied.stderr().contains("forward.connect"),
        "{}",
        denied.stderr()
    );
}

#[test]
fn forward_dash_l_listens_and_reaches_the_target() {
    let dir = tempfile::tempdir().expect("a directory");
    let (listener, port) = listening();
    let rule = format!("127.0.0.1:{port}");
    let (node, config, server, _peer) = node_and_peer(dir.path(), &[rule.as_str()]);
    let target = format!("127.0.0.1:{port}");
    let spec = format!("127.0.0.1:0={target}");
    let answered = serve_once(listener, |mut connection| {
        let mut request = Vec::new();
        connection.read_to_end(&mut request).expect("a request");
        connection
            .write_all(&[b"via -L: ", request.as_slice()].concat())
            .expect("a reply");
    });
    let mut args = remote_args(&node, &config, &server);
    args.extend(["forward", "-L", spec.as_str()]);
    let forward = Spawned::start(bohio(&args));
    let line = forward.line("local ");
    let (bound, shown_target) = line.split_once(" -> ").expect("local BOUND -> TARGET");
    assert_eq!(shown_target, target);
    let mut client = TcpStream::connect(bound).expect("the local port answers");
    client.write_all(b"query").expect("written");
    client
        .shutdown(std::net::Shutdown::Write)
        .expect("half-closed");
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).expect("a reply");
    assert_eq!(reply, b"via -L: query");
    answered.join().expect("the target ended");
    assert!(
        !String::from_utf8_lossy(&forward.stdout()).contains("non-default"),
        "loopback is the default"
    );
}

#[test]
fn forward_binds_to_loopback_by_default_and_says_when_another_address_is_named() {
    let dir = tempfile::tempdir().expect("a directory");
    let (_listener, port) = listening();
    let rule = format!("127.0.0.1:{port}");
    let (node, config, server, _peer) = node_and_peer(dir.path(), &[rule.as_str()]);
    let target = format!("127.0.0.1:{port}");
    let default = format!("0={target}");
    let mut args = remote_args(&node, &config, &server);
    args.extend(["forward", "-L", default.as_str()]);
    let forward = Spawned::start(bohio(&args));
    let line = forward.line("local ");
    assert!(line.starts_with("127.0.0.1:"), "{line}");

    let wide = format!("0.0.0.0:0={target}");
    let mut args = remote_args(&node, &config, &server);
    args.extend(["forward", "-L", wide.as_str()]);
    let named = Spawned::start(bohio(&args));
    assert_eq!(named.line("non-default: bind "), "0.0.0.0");
    named.line("local ");
}

#[test]
fn peer_approve_writes_connect_rules_and_a_bad_rule_is_named_by_config_check() {
    let dir = tempfile::tempdir().expect("a directory");
    let config = client_config(dir.path(), "host");
    let peer = id_of(&client_config(dir.path(), "other"));
    let path = config.to_string_lossy().into_owned();
    let output = bohio(&[
        "peer",
        "approve",
        &peer.to_string(),
        "--config",
        &path,
        "--allow",
        "info",
        "--connect",
        "db.lan:5432",
        "--connect",
        "10.0.0.0/8:22-25",
        "--connect",
        "unix:/run/docker.sock",
    ])
    .output()
    .expect("bohio runs");
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        said.contains("forward.connect")
            && said.contains("connect=db.lan:5432,10.0.0.0/8:22-25,unix:/run/docker.sock"),
        "{said}"
    );
    let text = std::fs::read_to_string(&config).expect("the file");
    assert!(
        text.contains(&format!("[peer.\"{peer}\".forward]")),
        "{text}"
    );
    assert!(text.contains("\"forward.connect\""), "{text}");
    let check = bohio(&["config", "check", "--config", &path])
        .output()
        .expect("bohio runs");
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );

    // A rule that cannot be read refuses the file, naming the key.
    let bad = text.replace("db.lan:5432", "10.0.0.0/33:22");
    std::fs::write(&config, bad).expect("written");
    let check = bohio(&["config", "check", "--config", &path])
        .output()
        .expect("bohio runs");
    assert!(!check.status.success());
    let error = String::from_utf8_lossy(&check.stderr);
    assert!(
        error.contains(&format!("peer.{peer}.forward.connect")),
        "{error}"
    );

    // A rule that cannot be read is refused at the command too.
    let refused = bohio(&[
        "peer",
        "approve",
        &peer.to_string(),
        "--config",
        &path,
        "--connect",
        "not a rule",
    ])
    .output()
    .expect("bohio runs");
    assert!(!refused.status.success());
}

/// A controller listening and a host that dials it, the host granting the controller `connect` targets: both `bohio serve`
/// children, linked.
struct Dialed {
    _dir: tempfile::TempDir,
    controller: support::procs::Node,
    host: support::procs::Node,
    _procs: Vec<support::procs::Proc>,
}

fn dialed(rule: &str) -> Dialed {
    use support::procs::{DEADLINE as UP, Node as TestNode};
    const BEAT: &str = "heartbeat = \"500ms\"\nheartbeat_misses = 3";
    const DIAL: &str = "retry_min = \"200ms\"\nretry_max = \"1s\"";
    let dir = tempfile::tempdir().expect("a directory");
    let controller = TestNode::new(dir.path(), "controller");
    let host = TestNode::new(dir.path(), "host");
    controller.write_with(BEAT, &TestNode::peer_table(&host.id, None, "", ""));
    let (controller_proc, listening) = controller.serve(Some("tcp://127.0.0.1:0"), "info");
    let (_, endpoint) = listening.expect("it listens");
    let grant = format!("allow = [\"forward.connect\"]\nforward = {{ connect = [\"{rule}\"] }}");
    host.write_with(
        BEAT,
        &TestNode::peer_table(&controller.id, Some(&endpoint), &grant, DIAL),
    );
    let (host_proc, _) = host.serve(None, "info");
    controller_proc.wait_stderr("link up", 1, UP);
    host_proc.wait_stderr("link up", 1, UP);
    Dialed {
        _dir: dir,
        controller,
        host,
        _procs: vec![controller_proc, host_proc],
    }
}

#[test]
fn connect_and_forward_work_over_a_link_the_host_made_through_via() {
    let (listener, port) = listening();
    let rule = format!("127.0.0.1:{port}");
    let linked = dialed(&rule);
    let config = linked.controller.config();
    let target = format!("127.0.0.1:{port}");
    let answered = serve_once(listener, |mut connection| {
        let mut request = Vec::new();
        connection.read_to_end(&mut request).expect("a request");
        connection
            .write_all(&[b"over via: ", request.as_slice()].concat())
            .expect("a reply");
    });

    let mut connect = Spawned::start(bohio(&[
        "remote",
        "--via",
        &linked.host.id,
        "--config",
        &config,
        "connect",
        &target,
    ]));
    let mut stdin = connect.child.stdin.take().expect("a pipe to stdin");
    stdin.write_all(b"bytes").expect("written");
    drop(stdin);
    assert_eq!(connect.finish(), Some(0), "{}", connect.stderr());
    assert_eq!(connect.stdout(), b"over via: bytes");
    answered.join().expect("the target ended");

    // The same target through `-L`, whose half-close crosses the node's relay both ways.
    let (listener, port) = listening();
    let rule = format!("127.0.0.1:{port}");
    let linked = dialed(&rule);
    let config = linked.controller.config();
    let target = format!("127.0.0.1:{port}");
    let answered = serve_once(listener, |mut connection| {
        let mut request = Vec::new();
        connection.read_to_end(&mut request).expect("a request");
        connection
            .write_all(&[b"-L over via: ", request.as_slice()].concat())
            .expect("a reply");
    });
    let spec = format!("127.0.0.1:0={target}");
    let forward = Spawned::start(bohio(&[
        "remote",
        "--via",
        &linked.host.id,
        "--config",
        &config,
        "forward",
        "-L",
        &spec,
    ]));
    let line = forward.line("local ");
    let (bound, _) = line.split_once(" -> ").expect("local BOUND -> TARGET");
    let mut client = TcpStream::connect(bound).expect("the local port answers");
    client.write_all(b"query").expect("written");
    client
        .shutdown(std::net::Shutdown::Write)
        .expect("half-closed");
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).expect("a reply");
    assert_eq!(reply, b"-L over via: query");
    answered.join().expect("the target ended");

    // A target the host's grant does not name is refused through the node too.
    let mut denied = Spawned::start(bohio(&[
        "remote",
        "--via",
        &linked.host.id,
        "--config",
        &config,
        "connect",
        "--check",
        "127.0.0.1:1",
    ]));
    assert_eq!(denied.finish(), Some(1));
    assert!(
        denied.stderr().contains("forward.connect"),
        "{}",
        denied.stderr()
    );
}

#[test]
fn forward_dash_l_listens_on_a_local_socket_or_pipe() {
    use coqui_transport_pipe::{PipeName, Stream};
    let dir = tempfile::tempdir().expect("a directory");
    let (listener, port) = listening();
    let rule = format!("127.0.0.1:{port}");
    let (node, config, server, _peer) = node_and_peer(dir.path(), &[rule.as_str()]);
    let target = format!("127.0.0.1:{port}");
    // The client of the local socket cannot half-close, so the target answers after the first five bytes.
    let answered = serve_once(listener, |mut connection| {
        let mut request = [0_u8; 5];
        connection.read_exact(&mut request).expect("a request");
        connection
            .write_all(&[b"local ipc: ", request.as_slice()].concat())
            .expect("a reply");
    });
    let local = if cfg!(windows) {
        format!("pipe:bohio-l-{}", std::process::id())
    } else {
        format!("unix:{}", dir.path().join("l.sock").display())
    };
    let spec = format!("{local}={target}");
    let mut args = remote_args(&node, &config, &server);
    args.extend(["forward", "-L", spec.as_str()]);
    let forward = Spawned::start(bohio(&args));
    let line = forward.line("local ");
    let (bound, shown_target) = line.split_once(" -> ").expect("local BOUND -> TARGET");
    assert_eq!(shown_target, target);
    let name: PipeName = bound.parse().expect("the printed name parses");
    let mut client = Stream::connect(&name).expect("the local socket answers");
    client.write_all(b"query").expect("written");
    let mut reply = Vec::new();
    let mut buffer = [0_u8; 64];
    let until = Instant::now() + WAIT;
    while reply.len() < b"local ipc: query".len() {
        assert!(Instant::now() < until, "no reply: {}", forward.stderr());
        match client.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => reply.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => panic!("{error}"),
        }
    }
    assert_eq!(reply, b"local ipc: query");
    answered.join().expect("the target ended");
}
