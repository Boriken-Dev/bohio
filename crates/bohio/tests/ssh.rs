//! A machine reached over SSH, served by a node as sources: a peer runs a program and attaches the console on it, with
//! the node's grants deciding what the peer may do. The machine is an in-process SSH server (`sh` runs its commands).

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::access::Sources;
use bohio::config::{AccessConfig, NodeConfig, SourceTables, Span};
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_client::Error;
use bohio_client::control::Control;
use bohio_client::exec::Exec;
use bohio_client::link::{Settings, stream};
use bohio_client::shell::Shell;
use bohio_proto::ErrorCode;
use bohio_proto::exec::Request;
use bohio_proto::shell::Request as ShellRequest;
use coqui_link::auth::{Identity, Method};
use coqui_transport::{Channel, StreamChannel, memory};

#[path = "../../../providers/ssh/tests/support/mod.rs"]
mod support;
use support::{FakeSsh, shell_available};

const WAIT: Duration = Duration::from_secs(60);

macro_rules! need_shell {
    () => {
        if !shell_available() {
            eprintln!("SKIPPED: no `sh` on PATH for the in-process SSH server to run commands");
            return;
        }
    };
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
    _host: Arc<Node>,
    _wire: Wire,
}

fn tables(text: &str) -> SourceTables {
    #[derive(serde::Deserialize)]
    struct Whole {
        source: SourceTables,
    }
    toml::from_str::<Whole>(text).expect("source tables").source
}

/// A node that serves the ssh source `remote` (the table `source_toml`) as its terminal and exec sources, and a
/// client granted `grant`, linked over memory pipes.
fn world(source_toml: &str, grant: Grant) -> World {
    let sources = Sources::unconfigured();
    sources
        .reconfigure(
            &tables(source_toml),
            &AccessConfig {
                terminal: Some(vec!["remote".to_owned()]),
                desktop: Some(Vec::new()),
                exec: Some(vec!["native".to_owned(), "remote".to_owned()]),
                fs: Some(Vec::new()),
                ..AccessConfig::default()
            },
        )
        .expect("the sources build");
    let make = |sources: Option<Sources>| {
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let mut node = Node::new(
            NodeConfig::default(),
            identity,
            keys.clone(),
            grants.clone(),
        )
        .expect("the node is built")
        .with_path(std::env::var_os("PATH").unwrap_or_default());
        if let Some(sources) = sources {
            node = node.with_sources(sources);
        }
        let _ = Span(Duration::ZERO);
        (Arc::new(node), id, keys, grants)
    };
    let (host, host_id, host_keys, host_grants) = make(Some(sources));
    let (client, client_id, client_keys, _) = make(None);
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
        _host: host,
        _wire: Wire {
            channels: [dial_end, answer_end],
            threads,
        },
    }
}

fn source_toml(server: &FakeSsh, extra: &str) -> String {
    format!(
        "[source.remote]\nprovider = \"ssh\"\nhost = \"127.0.0.1\"\nport = {}\nuser = \"{}\"\nidentity = {:?}\nhost_key = \"{}\"\nagent = false\n{extra}",
        server.port,
        support::USER,
        server.identity().to_string_lossy(),
        server.pin()
    )
}

fn granted() -> Grant {
    Grant::allowing([Permission::Info, Permission::Exec, Permission::Shell])
        .with_sources(["remote"])
}

#[test]
fn a_peer_runs_a_program_on_the_machine_through_the_ssh_source() {
    need_shell!();
    let server = FakeSsh::start();
    let world = world(&source_toml(&server, ""), granted());
    let mut request = Request::argv(["sh", "-c", "echo out; echo err >&2; exit 4"]);
    request.source = Some("remote".to_owned());
    let done = Exec::start(&world.link, &request)
        .expect("the stream opens")
        .wait(Some(WAIT))
        .expect("the program ends");
    assert_eq!(String::from_utf8_lossy(&done.stdout), "out\n");
    assert_eq!(String::from_utf8_lossy(&done.stderr), "err\n");
    assert_eq!(done.exit.code, Some(4));
    assert!(
        server
            .seen
            .lock()
            .unwrap()
            .commands
            .iter()
            .any(|command| command.contains("'echo out; echo err >&2; exit 4'")),
        "the machine ran it"
    );
}

#[test]
fn a_peer_attaches_the_machines_login_shell_as_the_console_of_the_source() {
    need_shell!();
    let server = FakeSsh::start();
    let world = world(&source_toml(&server, ""), granted());
    let shell = Shell::start(
        &world.link,
        &ShellRequest::console(Some("remote")),
        Some(WAIT),
    )
    .expect("the console opens");
    shell.write(b"echo from-$((6*7))\r").expect("typing");
    let heard = shell
        .read_until(b"from-42", Some(WAIT))
        .expect("the machine answers");
    assert!(String::from_utf8_lossy(&heard).contains("from-42"));
    assert!(
        server
            .seen
            .lock()
            .unwrap()
            .commands
            .contains(&"<shell>".to_owned())
    );
}

#[test]
fn info_lists_the_source_and_says_why_an_unreachable_one_is_not_available() {
    let server = FakeSsh::start();
    let world = world(&source_toml(&server, ""), granted());
    let info = Control::open(&world.link)
        .expect("control")
        .info(Some(WAIT))
        .expect("info");
    let remote = info
        .access
        .exec
        .iter()
        .find(|entry| entry.name == "remote")
        .expect("the source is listed");
    assert_eq!(remote.provider, "ssh");
    assert!(remote.available, "{remote:?}");

    // The same source on a port nothing listens on: the node starts, and `info` says why.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let dead = source_toml(&server, "").replace(&server.port.to_string(), &port.to_string());
    let world = self::world(&dead, granted());
    let info = Control::open(&world.link)
        .expect("control")
        .info(Some(WAIT))
        .expect("info");
    let remote = info
        .access
        .exec
        .iter()
        .find(|entry| entry.name == "remote")
        .expect("the source is listed");
    assert!(!remote.available);
    assert!(
        remote
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains(&port.to_string())),
        "{remote:?}"
    );
    // A program on it is refused, not hung.
    let mut request = Request::argv(["true"]);
    request.source = Some("remote".to_owned());
    match Exec::start(&world.link, &request)
        .expect("the stream opens")
        .wait(Some(WAIT))
    {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::Unavailable),
        other => panic!("expected the host's refusal, got {other:?}"),
    }
}

#[test]
fn a_peer_not_granted_the_source_cannot_run_a_program_on_it() {
    need_shell!();
    let server = FakeSsh::start();
    let world = world(
        &source_toml(&server, ""),
        Grant::allowing([Permission::Info, Permission::Exec]),
    );
    let mut request = Request::argv(["true"]);
    request.source = Some("remote".to_owned());
    let refused = Exec::start(&world.link, &request)
        .map(drop)
        .expect_err("the source is not the peer's");
    assert!(
        matches!(&refused, Error::Transport(_))
            || matches!(&refused, Error::Remote(body) if body.code == ErrorCode::Denied),
        "{refused:?}"
    );
    assert!(server.seen.lock().unwrap().commands.is_empty());
}
