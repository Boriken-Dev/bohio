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
                terminal: Some(vec!["native".to_owned(), "remote".to_owned()]),
                desktop: Some(Vec::new()),
                exec: Some(vec!["native".to_owned(), "remote".to_owned()]),
                fs: Some(vec!["native".to_owned(), "remote".to_owned()]),
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
    shell.write(b"echo from-$((6*7))\n").expect("typing");
    let heard = shell
        .read_until(b"from-42", Some(Duration::from_secs(20)))
        .unwrap_or_else(|error| {
            panic!(
                "the machine does not answer ({error:?}); it saw {:?}",
                server.seen.lock().unwrap()
            )
        });
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
    // The link refuses the open itself, or the engine answers `denied`.
    let refused = Exec::start(&world.link, &request)
        .and_then(|exec| exec.wait(Some(WAIT)).map(drop))
        .expect_err("the source is not the peer's");
    assert!(
        matches!(&refused, Error::Transport(_))
            || matches!(&refused, Error::Remote(body) if body.code == ErrorCode::Denied),
        "{refused:?}"
    );
    assert!(server.seen.lock().unwrap().commands.is_empty());
}

/// A directory as the machine's shell names it, and so as the files source of a POSIX path does (`/c/...` for a drive).
fn posix(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    match text.split_once(":/") {
        Some((drive, rest)) if drive.len() == 1 => {
            format!("/{}/{rest}", drive.to_ascii_lowercase())
        }
        _ => text,
    }
}

fn files_granted(root: &str) -> Grant {
    Grant::allowing([Permission::Info, Permission::FsRead, Permission::FsWrite])
        .with_sources(["remote"])
        .with_fs_roots([root.to_owned()])
}

#[test]
fn a_peer_reads_and_changes_the_machines_files_through_the_ssh_source() {
    let server = FakeSsh::start();
    let machine = tempfile::tempdir().unwrap();
    let root = posix(machine.path());
    let world = world(&source_toml(&server, ""), files_granted(&root));
    let remote = bohio_client::Sourced::new(&world.link, "remote");
    let fs = bohio_client::fs::Fs::connect(&remote).expect("the control stream opens");

    fs.mkdir(&format!("{root}/work"), false).expect("mkdir");
    assert!(machine.path().join("work").is_dir(), "the machine has it");

    // A file goes up with a transfer and comes back the same.
    let here = tempfile::tempdir().unwrap();
    let content: Vec<u8> = (0..300_000_u32).map(|n| (n % 251) as u8).collect();
    std::fs::write(here.path().join("up.bin"), &content).unwrap();
    bohio_client::sync::put(
        &remote,
        &here.path().join("up.bin"),
        &format!("{root}/work/up.bin"),
        &bohio_client::sync::PutOptions::default(),
    )
    .expect("put");
    assert_eq!(
        std::fs::read(machine.path().join("work/up.bin")).unwrap(),
        content
    );
    let stat = fs.stat(&format!("{root}/work/up.bin"), true).expect("stat");
    assert_eq!(stat.size, content.len() as u64);

    let listed = fs.list_all(&format!("{root}/work")).expect("list");
    assert_eq!(
        listed
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["up.bin"]
    );

    fs.rename(
        &format!("{root}/work/up.bin"),
        &format!("{root}/work/moved.bin"),
        false,
    )
    .expect("rename");
    bohio_client::sync::get(
        &remote,
        &format!("{root}/work/moved.bin"),
        &here.path().join("down.bin"),
        &bohio_client::sync::GetOptions::default(),
    )
    .expect("get");
    assert_eq!(
        std::fs::read(here.path().join("down.bin")).unwrap(),
        content
    );

    // A read stream and a hash name the source as well.
    let reader = bohio_client::fs::Reader::start(
        &remote,
        &bohio_proto::fs::ReadRequest::whole(format!("{root}/work/moved.bin")),
        Some(WAIT),
    )
    .expect("read");
    assert_eq!(reader.read_to_end(Some(WAIT)).expect("bytes"), content);

    fs.remove(&format!("{root}/work"), true).expect("remove");
    assert!(!machine.path().join("work").exists());
}

#[test]
fn a_peer_is_held_to_its_roots_on_the_machine_and_to_the_sources_it_holds() {
    let server = FakeSsh::start();
    let machine = tempfile::tempdir().unwrap();
    std::fs::create_dir(machine.path().join("inside")).unwrap();
    std::fs::create_dir(machine.path().join("outside")).unwrap();
    let granted_root = posix(&machine.path().join("inside"));
    let world = world(&source_toml(&server, ""), files_granted(&granted_root));
    let remote = bohio_client::Sourced::new(&world.link, "remote");
    let fs = bohio_client::fs::Fs::connect(&remote).unwrap();
    fs.stat(&granted_root, true).expect("inside the root");
    for path in [
        posix(&machine.path().join("outside")),
        format!("{granted_root}/../outside"),
        "/etc/passwd".to_owned(),
    ] {
        match fs.stat(&path, true) {
            Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::OutsideRoot, "{path}"),
            other => panic!("{path}: expected outside_root, got {other:?}"),
        }
    }

    // Without the source in its grant the peer is refused, and the machine is not touched.
    let world = self::world(
        &source_toml(&server, ""),
        Grant::allowing([Permission::Info, Permission::FsRead, Permission::FsWrite])
            .with_fs_roots([granted_root.clone()]),
    );
    let remote = bohio_client::Sourced::new(&world.link, "remote");
    let fs = bohio_client::fs::Fs::connect(&remote).unwrap();
    match fs.mkdir(&format!("{granted_root}/made"), false) {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::Denied),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(!machine.path().join("inside/made").exists());
}

#[test]
fn info_lists_the_files_source_with_what_it_lacks() {
    let server = FakeSsh::start();
    let machine = tempfile::tempdir().unwrap();
    let world = world(
        &source_toml(&server, ""),
        files_granted(&posix(machine.path())),
    );
    let info = Control::open(&world.link)
        .unwrap()
        .info(Some(WAIT))
        .unwrap();
    let remote = info
        .access
        .fs
        .iter()
        .find(|entry| entry.name == "remote")
        .expect("listed");
    assert!(remote.available, "{remote:?}");
    assert!(remote.lacks.iter().any(|word| word == "confined"));
    assert!(remote.offers.iter().any(|word| word == "posix_paths"));
}

/// A measurement, not a check: a file goes through a node to the `native` source and to an ssh source of a real server on
/// this machine, alternately, and the time of each is printed. It needs `BOHIO_BENCH_SSH` (`PORT,USER,IDENTITY,PIN,SERVER_DIR`)
/// and `BOHIO_BENCH_DIR` (a directory of this machine), and runs with `-- --ignored --nocapture`.
#[test]
#[ignore = "a measurement against a real server; see the test's comment"]
fn a_large_transfer_to_the_ssh_source_against_the_native_source() {
    let (Ok(spec), Ok(dir)) = (
        std::env::var("BOHIO_BENCH_SSH"),
        std::env::var("BOHIO_BENCH_DIR"),
    ) else {
        panic!("BOHIO_BENCH_SSH and BOHIO_BENCH_DIR are not set");
    };
    let parts: Vec<&str> = spec.split(',').collect();
    let [port, user, identity, pin, server_dir] = parts[..] else {
        panic!("BOHIO_BENCH_SSH is PORT,USER,IDENTITY,PIN,SERVER_DIR");
    };
    let bytes: u64 = std::env::var("BOHIO_BENCH_BYTES")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(1024 * 1024 * 1024);
    let runs: usize = std::env::var("BOHIO_BENCH_RUNS")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(3);
    let native_dir = format!("{dir}/native");
    let ssh_dir = format!("{server_dir}/scratch");
    std::fs::create_dir_all(&native_dir).unwrap();
    let source = format!(
        "[source.remote]\nprovider = \"ssh\"\nhost = \"127.0.0.1\"\nport = {port}\nuser = \"{user}\"\nidentity = {identity:?}\nhost_key = \"{pin}\"\nagent = false\n"
    );
    let granted = || {
        Grant::allowing([Permission::Info, Permission::FsRead, Permission::FsWrite])
            .with_sources(["remote"])
            .with_fs_roots([native_dir.clone(), ssh_dir.clone()])
    };
    let local = format!("{dir}/source.bin");
    {
        use std::io::Write as _;
        let mut file = std::io::BufWriter::new(std::fs::File::create(&local).unwrap());
        let mut block = vec![0_u8; 1024 * 1024];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut written = 0_u64;
        while written < bytes {
            for chunk in block.chunks_mut(8) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let word = state.to_le_bytes();
                chunk.copy_from_slice(&word[..chunk.len()]);
            }
            let take = usize::try_from((bytes - written).min(block.len() as u64)).unwrap();
            file.write_all(&block[..take]).unwrap();
            written += take as u64;
        }
        file.flush().unwrap();
    }
    let mib = |seconds: f64| bytes as f64 / 1_048_576.0 / seconds;
    let median = |mut times: Vec<f64>| {
        times.sort_by(f64::total_cmp);
        (times[times.len() / 2], times[0], times[times.len() - 1])
    };
    for (label, extra) in [
        ("sftp default", ""),
        (
            "sftp tuned (64 in flight, 128 KiB)",
            "sftp_in_flight = 64\nsftp_packet = 131072\n",
        ),
    ] {
        let world = world(&format!("{source}{extra}"), granted());
        let ssh = bohio_client::Sourced::new(&world.link, "remote");
        let mut native_up = Vec::new();
        let mut ssh_up = Vec::new();
        let mut native_down = Vec::new();
        let mut ssh_down = Vec::new();
        for _ in 0..runs {
            for (name, link, target, up, down) in [
                (
                    "native",
                    &world.link as &dyn bohio_client::Open,
                    format!("{native_dir}/x.bin"),
                    &mut native_up,
                    &mut native_down,
                ),
                (
                    "ssh",
                    &ssh as &dyn bohio_client::Open,
                    format!("{ssh_dir}/x.bin"),
                    &mut ssh_up,
                    &mut ssh_down,
                ),
            ] {
                let began = Instant::now();
                bohio_client::sync::put(
                    link,
                    std::path::Path::new(&local),
                    &target,
                    &bohio_client::sync::PutOptions::default(),
                )
                .unwrap_or_else(|error| panic!("{name} put: {error:?}"));
                up.push(began.elapsed().as_secs_f64());
                let back = format!("{dir}/back.bin");
                let began = Instant::now();
                bohio_client::sync::get(
                    link,
                    &target,
                    std::path::Path::new(&back),
                    &bohio_client::sync::GetOptions::default(),
                )
                .unwrap_or_else(|error| panic!("{name} get: {error:?}"));
                down.push(began.elapsed().as_secs_f64());
                assert_eq!(std::fs::metadata(&back).unwrap().len(), bytes);
                let _ = std::fs::remove_file(&back);
                let _ = std::fs::remove_file(&target);
            }
        }
        for (name, what, times) in [
            ("native", "put", native_up),
            ("ssh", "put", ssh_up),
            ("native", "get", native_down),
            ("ssh", "get", ssh_down),
        ] {
            let (mid, low, high) = median(times);
            println!(
                "bench [{label}] {name} {what} {} MiB: median {mid:.2}s ({:.0} MiB/s), best {low:.2}s, worst {high:.2}s over {runs}",
                bytes / 1_048_576,
                mib(mid)
            );
        }
    }
    let _ = std::fs::remove_file(&local);
}
