//! A node embedded in a test: its settings, identity, keys and grants are values in memory, with no file
//! and no `bohio` process on the host side. The built `bohio` is the peer that uses it.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::config::{LinkConfig, ListenConfig, NodeConfig, Span};
use bohio::listen::Listening;
use bohio::node::{MemoryGrants, MemoryKeys, Node};
use bohio::policy::{Grant, Permission};
use bohio_client::Error;
use bohio_client::connect::Connection;
use bohio_client::exec::{Event as ExecEvent, Exec};
use bohio_client::link::Settings;
use bohio_proto::exec::Request;
use coqui_link::auth::{Identity, Method, Proof, Trust};
use coqui_peer::PeerId;
use coqui_transport::Failure;
use support::{alive, gone_within, helper};

/// A wait long enough for any exchange on this machine, short enough to fail a hung test.
const DEADLINE: Duration = Duration::from_secs(60);

/// The node's re-check interval in these tests.
const RECHECK: Duration = Duration::from_millis(500);

/// What scheduling and a busy machine may add to "within `2 x recheck`".
const SLACK: Duration = Duration::from_secs(3);

/// A running node, stopped and joined when dropped.
struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    endpoint: String,
    address: std::net::SocketAddr,
    id: PeerId,
    keys: Arc<MemoryKeys>,
    grants: Arc<MemoryGrants>,
}

impl Running {
    fn start() -> Self {
        let config = NodeConfig {
            link: LinkConfig {
                recheck: Span(RECHECK),
                ..LinkConfig::default()
            },
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
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A `bohio` configuration naming a data directory of its own.
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
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    command
}

/// What a finished `bohio` left.
struct Done {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn finished(mut child: Child) -> Done {
    let until = Instant::now() + DEADLINE;
    while child.try_wait().expect("the child can be asked").is_none() {
        assert!(Instant::now() < until, "bohio did not end in time");
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().expect("its output");
    Done {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn run(args: &[&str]) -> Done {
    finished(
        bohio(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("bohio starts"),
    )
}

/// The id of the peer `config` names, its identity made on first use.
fn id_of(config: &Path) -> PeerId {
    let done = run(&["id", "--config", &config.to_string_lossy()]);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    done.stdout.trim().parse().expect("a peer id")
}

#[test]
fn a_node_built_from_memory_serves_bohio_info_exec_and_read() {
    let node = Running::start();
    let dir = tempfile::tempdir().expect("a directory");
    let root = dir.path().join("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("hello.txt"), "read from an embedded node\n").unwrap();
    let config = client_config(dir.path(), "peer");
    let peer = id_of(&config);
    node.keys.accept(peer);
    node.grants.set(
        peer,
        Grant::allowing([Permission::Info, Permission::Exec, Permission::FsRead])
            .with_fs_roots([&root]),
    );
    let (config, server) = (config.to_string_lossy().into_owned(), node.id.to_string());
    let remote = |action: &[&str]| {
        let mut args = vec![
            "remote",
            node.endpoint.as_str(),
            "--config",
            config.as_str(),
            "--server",
            server.as_str(),
        ];
        args.extend_from_slice(action);
        run(&args)
    };

    let info = remote(&["info", "--format", "json"]);
    assert_eq!(info.code, Some(0), "{}", info.stderr);
    assert!(
        info.stdout.contains(&format!("\"host_id\":\"{server}\"")),
        "{}",
        info.stdout
    );
    let echo = remote(&["exec", "--", &helper(), "echo", "from", "memory"]);
    assert_eq!(echo.code, Some(0), "{}", echo.stderr);
    assert_eq!(echo.stdout.trim(), "from memory");
    let path = root.join("hello.txt").to_string_lossy().into_owned();
    let read = remote(&["read", &path]);
    assert_eq!(read.code, Some(0), "{}", read.stderr);
    assert_eq!(read.stdout, "read from an embedded node\n");

    // The grant is the whole authority: the same peer may not write what its grant does not hold.
    node.grants.set(peer, Grant::allowing([Permission::Info]));
    let refused = remote(&["exec", "--", &helper(), "echo", "no"]);
    assert_ne!(refused.code, Some(0), "an exec the grant lacks ran");
}

#[test]
fn removing_a_grant_from_memory_ends_a_running_exec_within_two_rechecks() {
    let node = Running::start();
    let dir = tempfile::tempdir().expect("a directory");
    let config = client_config(dir.path(), "peer");
    let peer = id_of(&config);
    node.keys.accept(peer);
    node.grants.set(peer, Grant::allowing([Permission::Exec]));

    // The command-line peer waits on a long helper; a library peer sees the process itself.
    let long = bohio(&[
        "remote",
        &node.endpoint,
        "--config",
        &config.to_string_lossy(),
        "--server",
        &node.id.to_string(),
        "exec",
        "--",
        &helper(),
        "tree",
        "120000",
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("bohio starts");

    let identity = Identity::generate(Method::DEFAULT).expect("an identity");
    let library_peer = identity.peer_id();
    node.keys.accept(library_peer);
    node.grants
        .set(library_peer, Grant::allowing([Permission::Exec]));
    let proof = Proof::builder(identity)
        .responders(Trust::pinned([node.id]))
        .build()
        .expect("a proof");
    let link = Connection::connect_tcp(
        node.address,
        Duration::from_secs(10),
        &proof,
        &Settings::default(),
    )
    .expect("the link is up");
    let exec = Exec::start(&link, &Request::argv([helper().as_str(), "tree", "120000"])).unwrap();
    let Ok(Some(ExecEvent::Started { pid })) = exec.next(Some(Duration::from_secs(20))) else {
        panic!("the process did not start");
    };
    assert!(alive(pid));
    // Let the command-line peer's helper start as well before the grants go.
    thread::sleep(Duration::from_secs(1));

    node.grants.remove(&peer);
    node.grants.remove(&library_peer);
    let started = Instant::now();
    let error = loop {
        match exec.next(Some(Duration::from_millis(100))) {
            Err(Error::Timeout) | Ok(_) => {}
            Err(error) => break error,
        }
        assert!(
            started.elapsed() < DEADLINE,
            "the exec was not ended by the removed grant"
        );
    };
    let took = started.elapsed();
    assert!(
        matches!(error, Error::Transport(Failure::Permission)),
        "the stream was not ended by the admission: {error:?}"
    );
    assert!(took <= RECHECK * 2 + SLACK, "took {took:?}");
    assert!(
        gone_within(pid, Duration::from_secs(15)),
        "the process of a peer that lost its grant is still running"
    );

    let done = finished(long);
    assert_ne!(done.code, Some(0));
    assert!(
        done.stderr.contains("revoked"),
        "the command did not say its access was revoked: {}",
        done.stderr
    );
}
