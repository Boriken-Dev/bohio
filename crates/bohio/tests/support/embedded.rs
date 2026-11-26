//! An embedded node, library peers and the built binary, for the tests that forward bytes.
#![allow(dead_code, reason = "each test binary uses some of these")]

use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{ForwardConfig, LinkConfig, ListenConfig, NodeConfig, Span};
use bohio::listen::Listening;
use bohio::node::{MemoryGrants, MemoryKeys, Node};
use bohio::policy::Grant;
use bohio_client::connect::Connection;
use bohio_client::link::Settings;
use cabuya::auth::{Identity, Method, Proof, Trust};
use cabuya::mux::LinkStream;
use coqui_peer::PeerId;

/// A wait long enough for any exchange on this machine, short enough to fail a hung test.
pub const WAIT: Duration = Duration::from_secs(20);

/// The node's re-check interval in these tests.
pub const RECHECK: Duration = Duration::from_millis(500);

/// What scheduling and a busy machine may add to "within `2 x recheck`".
pub const SLACK: Duration = Duration::from_secs(3);

/// How long one forwarding test may run before it is ended as hung.
pub const DEADLINE: Duration = Duration::from_secs(300);

/// Ends the whole test process, naming the test, when the test that holds it is still running after [`DEADLINE`]: a forward
/// that waits on a peer that will never answer fails the run at once, with the name of what hung, instead of holding it until
/// the job's own limit. A test names itself through its thread, which the harness names after it.
pub struct Deadline(mpsc::Sender<()>);

impl Deadline {
    pub fn start() -> Self {
        let name = thread::current().name().unwrap_or("a test").to_owned();
        let (done, waiting) = mpsc::channel::<()>();
        thread::spawn(move || {
            if waiting.recv_timeout(DEADLINE) == Err(mpsc::RecvTimeoutError::Timeout) {
                eprintln!("{name} was still running after {DEADLINE:?}: it is hung");
                std::process::exit(101);
            }
        });
        Self(done)
    }
}

/// A running node, stopped and joined when dropped.
pub struct Running {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
    pub endpoint: String,
    pub address: SocketAddr,
    pub id: PeerId,
    pub keys: Arc<MemoryKeys>,
    pub grants: Arc<MemoryGrants>,
}

impl Running {
    pub fn start() -> Self {
        Self::with(ForwardConfig::default())
    }

    pub fn with(forward: ForwardConfig) -> Self {
        Self::with_link(forward, LinkConfig::default())
    }

    /// As [`Running::with`], with `link` as the link's settings (its re-check interval is the tests').
    pub fn with_link(forward: ForwardConfig, link: LinkConfig) -> Self {
        Self::with_node(NodeConfig {
            link: LinkConfig {
                recheck: Span(RECHECK),
                ..link
            },
            forward,
            ..NodeConfig::default()
        })
    }

    /// A node with exactly `config`.
    pub fn with_node(config: NodeConfig) -> Self {
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
    pub fn link(&self, grant: Grant) -> (Connection, PeerId) {
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let peer = identity.peer_id();
        self.keys.accept(peer);
        self.grants.set(peer, grant);
        (self.link_as(&identity, self.address), peer)
    }

    /// A link to the node as `identity` (accepted and granted by the caller), dialing `address`: the node's own, or a
    /// [`Proxy`]'s in front of it.
    pub fn link_as(&self, identity: &Identity, address: SocketAddr) -> Connection {
        let proof = Proof::builder(identity.clone())
            .responders(Trust::pinned([self.id]))
            .build()
            .expect("a proof");
        Connection::connect_tcp(
            address,
            Duration::from_secs(10),
            &proof,
            &Settings::default(),
        )
        .expect("the link is up")
    }

    /// A new identity the node accepts, holding `grant`.
    pub fn peer(&self, grant: Grant) -> Identity {
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        self.keys.accept(identity.peer_id());
        self.grants.set(identity.peer_id(), grant);
        identity
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

/// A TCP relay in front of a node that can be cut the way a lost network cuts a link: both of its connections close at
/// once and nothing says goodbye to the link's own streams.
pub struct Proxy {
    pub address: SocketAddr,
    connections: Arc<Mutex<Vec<TcpStream>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Proxy {
    pub fn to(target: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        listener.set_nonblocking(true).expect("non-blocking");
        let address = listener.local_addr().expect("an address");
        let connections: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let (connections, stop) = (Arc::clone(&connections), Arc::clone(&stop));
            move || {
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
                    let mut held = connections.lock().unwrap();
                    for (mut source, mut sink) in [
                        (from.try_clone().unwrap(), to.try_clone().unwrap()),
                        (to.try_clone().unwrap(), from.try_clone().unwrap()),
                    ] {
                        thread::spawn(move || {
                            let _ = std::io::copy(&mut source, &mut sink);
                            let _ = sink.shutdown(std::net::Shutdown::Both);
                        });
                    }
                    held.push(from);
                    held.push(to);
                }
            }
        });
        Self {
            address,
            connections,
            stop,
            thread: Some(thread),
        }
    }

    /// Cuts every connection through the proxy, at the sockets and without a word to the streams on them.
    pub fn sever(&self) {
        for connection in self.connections.lock().unwrap().drain(..) {
            let _ = connection.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.sever();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A listener on a free loopback port, and the port.
pub fn listening() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    (listener, port)
}

/// A port nothing listens on.
pub fn closed_port() -> u16 {
    listening().1
}

/// Serves one connection with `serve` on a thread of its own.
pub fn serve_once(
    listener: TcpListener,
    serve: impl FnOnce(TcpStream) + Send + 'static,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let (connection, _) = listener.accept().expect("a connection");
        serve(connection);
    })
}

/// Reads until the end of the stream, which has to be a `finish`.
pub fn read_all(stream: &dyn LinkStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        match stream.receive(Some(WAIT)) {
            Ok(Some(message)) => bytes.extend_from_slice(&message),
            Ok(None) => return bytes,
            Err(failure) => panic!("the stream failed: {failure:?}"),
        }
    }
}

pub fn client_config(dir: &Path, name: &str) -> PathBuf {
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

pub fn bohio(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bohio"));
    command
        .args(args)
        .env_remove("BOHIO_CONFIG")
        .env("BOHIO_LOG", "warn")
        .env("NO_COLOR", "1");
    command
}

pub fn id_of(config: &Path) -> PeerId {
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
pub struct Spawned {
    pub child: Child,
    pub stdout: Arc<Mutex<Vec<u8>>>,
    pub stderr: Arc<Mutex<Vec<u8>>>,
}

pub fn collect(mut source: impl Read + Send + 'static) -> Arc<Mutex<Vec<u8>>> {
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
    pub fn start(mut command: Command) -> Self {
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

    pub fn stdout(&self) -> Vec<u8> {
        self.stdout.lock().unwrap().clone()
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }

    /// Waits for the child to end and for the rest of its output.
    pub fn finish(&mut self) -> Option<i32> {
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
    pub fn line(&self, prefix: &str) -> String {
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
pub fn remote_args<'a>(node: &'a Running, config: &'a str, server: &'a str) -> Vec<&'a str> {
    vec![
        "remote",
        node.endpoint.as_str(),
        "--config",
        config,
        "--server",
        server,
    ]
}

/// A controller listening and a host that dials it, the host granting the controller `connect` targets: both `bohio serve`
/// children, linked.
pub struct Dialed {
    _dir: tempfile::TempDir,
    pub controller: super::procs::Node,
    pub host: super::procs::Node,
    _procs: Vec<super::procs::Proc>,
}

pub fn dialed_with(grant: &str) -> Dialed {
    use super::procs::{DEADLINE as UP, Node as TestNode};
    const BEAT: &str = "heartbeat = \"500ms\"\nheartbeat_misses = 3";
    const DIAL: &str = "retry_min = \"200ms\"\nretry_max = \"1s\"";
    let dir = super::private_dir::private_tempdir();
    let controller = TestNode::new(dir.path(), "controller");
    let host = TestNode::new(dir.path(), "host");
    controller.write_with(BEAT, &TestNode::peer_table(&host.id, None, "", ""));
    let (controller_proc, listening) = controller.serve(Some("tcp://127.0.0.1:0"), "info");
    let (_, endpoint) = listening.expect("it listens");
    host.write_with(
        BEAT,
        &TestNode::peer_table(&controller.id, Some(&endpoint), grant, DIAL),
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
