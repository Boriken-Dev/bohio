//! The authenticated link end to end: `bohio serve` runs as a child process on `127.0.0.1:0`, peers pair
//! and prove themselves to it, and the host decides what each may do from the `[peer.<id>]` tables of its
//! `bohio.cfg` layers.
//!
//! Every wait has a deadline, every child is killed when its handle drops, and every data directory is
//! under the system temporary directory.

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::peer::Data;
use bohio_client::Error;
use bohio_client::connect::Connection;
use bohio_client::exec::{Event as ExecEvent, Exec};
use bohio_client::fs::{Reader, Writer};
use bohio_client::link::Settings;
use bohio_client::shell::{Event as ShellEvent, Shell};
use bohio_proto::exec::Request;
use bohio_proto::fs::{ReadRequest, WriteRequest};
use bohio_proto::shell::{Request as ShellRequest, ShellKind};
use coqui_link::auth::{Proof, Trust};
use coqui_transport::Failure;
use sha2::{Digest, Sha256};
use support::{alive, gone_within, helper};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tempfile::TempDir;

/// A wait long enough for any exchange on this machine, short enough to fail a hung test.
const DEADLINE: Duration = Duration::from_secs(60);

/// The host's re-check interval in these tests.
const RECHECK: Duration = Duration::from_secs(1);

/// What scheduling and a busy machine may add to "within `recheck`".
const SLACK: Duration = Duration::from_secs(3);

/// A child process whose output is read on threads and which is killed when dropped.
struct Proc {
    child: Child,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    readers: Vec<JoinHandle<()>>,
}

/// What a finished child left.
struct Finished {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn collect(mut source: impl Read + Send + 'static) -> (Arc<Mutex<Vec<u8>>>, JoinHandle<()>) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&bytes);
    let handle = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(count) = source.read(&mut buffer) {
            if count == 0 {
                break;
            }
            shared.lock().unwrap().extend_from_slice(&buffer[..count]);
        }
    });
    (bytes, handle)
}

fn text(bytes: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&bytes.lock().unwrap()).into_owned()
}

impl Proc {
    /// Starts `bohio` with `args`, `log` as its log filter and `stdin` as its input (closed when
    /// given, empty otherwise).
    fn start(args: &[&str], stdin: Option<&str>, log: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bohio"));
        command
            .args(args)
            .env_remove("BOHIO_CONFIG")
            .env("BOHIO_LOG", log)
            .env("NO_COLOR", "1")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("the binary starts");
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input.as_bytes()).expect("input is written");
        }
        let (stdout, out_reader) = collect(child.stdout.take().unwrap());
        let (stderr, err_reader) = collect(child.stderr.take().unwrap());
        Self {
            child,
            stdout,
            stderr,
            readers: vec![out_reader, err_reader],
        }
    }

    /// The rest of the first standard-output line that starts with `prefix`, waiting for it.
    fn expect_line(&mut self, prefix: &str) -> String {
        let until = Instant::now() + DEADLINE;
        loop {
            for line in text(&self.stdout).lines() {
                if let Some(rest) = line.strip_prefix(prefix) {
                    return rest.trim().to_owned();
                }
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                // Its last output may still be in a reader thread: look once more before giving up.
                thread::sleep(Duration::from_millis(300));
                let out = text(&self.stdout);
                if let Some(rest) = out.lines().find_map(|line| line.strip_prefix(prefix)) {
                    return rest.trim().to_owned();
                }
                panic!(
                    "the child ended ({status}) before printing `{prefix}`\nstdout: {out}\nstderr: {}",
                    text(&self.stderr)
                );
            }
            assert!(
                Instant::now() < until,
                "no `{prefix}` line within the deadline\nstdout: {}",
                text(&self.stdout)
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the child to end, killing it and failing at the deadline.
    fn finish(mut self) -> Finished {
        let until = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("the child can be asked") {
                break status;
            }
            if Instant::now() >= until {
                let _ = self.child.kill();
                panic!(
                    "the child did not end within the deadline\nstdout: {}\nstderr: {}",
                    text(&self.stdout),
                    text(&self.stderr)
                );
            }
            thread::sleep(Duration::from_millis(20));
        };
        for reader in std::mem::take(&mut self.readers) {
            let until = Instant::now() + Duration::from_secs(5);
            while !reader.is_finished() && Instant::now() < until {
                thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
        Finished {
            code: status.code(),
            stdout: text(&self.stdout),
            stderr: text(&self.stderr),
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(args: &[&str]) -> Finished {
    Proc::start(args, None, "warn").finish()
}

fn run_ok(args: &[&str]) -> Finished {
    let done = run(args);
    assert_eq!(
        done.code,
        Some(0),
        "bohio {args:?} failed\nstdout: {}\nstderr: {}",
        done.stdout,
        done.stderr
    );
    done
}

/// A peer's side of the tests: its own configuration and data directory.
struct Client {
    config: String,
    data: Data,
    id: String,
}

/// A host and where the tests keep its data.
struct World {
    dir: TempDir,
    config: String,
}

/// A running `bohio serve`.
struct Host {
    process: Proc,
    address: SocketAddr,
    endpoint: String,
    id: String,
}

fn toml_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

impl World {
    /// A host whose `[link]` section is `recheck = "1s"` and `link_extra`.
    fn new(link_extra: &str) -> Self {
        let dir = tempfile::tempdir().expect("a directory");
        let data = dir.path().join("host-data");
        let config = dir.path().join("host.cfg");
        std::fs::write(
            &config,
            format!(
                "[service]\nname = \"test-host\"\ndata_dir = {}\n[link]\nrecheck = \"1s\"\n{link_extra}\n",
                toml_path(&data)
            ),
        )
        .expect("the configuration is written");
        Self {
            config: config.to_string_lossy().into_owned(),
            dir,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Starts `bohio serve` and waits for its port.
    fn serve(&self, log: &str) -> Host {
        self.serve_layers(&["--config", &self.config], log)
    }

    /// Starts `bohio serve` with `layers` (`--config` arguments) and waits for its port.
    fn serve_layers(&self, layers: &[&str], log: &str) -> Host {
        let mut args = vec!["serve"];
        args.extend_from_slice(layers);
        args.extend_from_slice(&["--listen", "tcp://127.0.0.1:0"]);
        let mut process = Proc::start(&args, None, log);
        let endpoint = process.expect_line("listening ");
        let id = process.expect_line("host ");
        let address: SocketAddr = endpoint
            .strip_prefix("tcp://")
            .expect("a tcp endpoint")
            .parse()
            .expect("an address");
        assert!(reachable(address), "the host does not answer on {address}");
        Host {
            process,
            address,
            endpoint,
            id,
        }
    }

    /// A peer with its own data directory.
    fn client(&self, name: &str) -> Client {
        let data = self.path(&format!("{name}-data"));
        let config = self.path(&format!("{name}.cfg"));
        std::fs::write(
            &config,
            format!(
                "[service]\nname = \"{name}\"\ndata_dir = {}\n[link]\nrecheck = \"1s\"\n",
                toml_path(&data)
            ),
        )
        .expect("the configuration is written");
        let config = config.to_string_lossy().into_owned();
        let id = run_ok(&["id", "--config", &config])
            .stdout
            .trim()
            .to_owned();
        Client {
            config,
            data: Data::new(data),
            id,
        }
    }

    /// Pairs `client` with the host by a PIN: the host shows it, the client joins with it.
    fn pair(&self, client: &Client) {
        let mut door = Proc::start(
            &["pair", "--config", &self.config, "--timeout", "60s"],
            None,
            "warn",
        );
        let endpoint = door.expect_line("pairing on ");
        let pin = door.expect_line("pin ");
        run_ok(&["join", &endpoint, "--pin", &pin, "--config", &client.config]);
        let done = door.finish();
        assert_eq!(done.code, Some(0), "{}", done.stderr);
        assert!(
            done.stdout.contains(&format!("paired {}", client.id)),
            "{}",
            done.stdout
        );
    }

    /// `bohio peer approve` for `client` with `grant` flags.
    fn approve(&self, client: &Client, grant: &[&str]) -> Finished {
        let mut args = vec!["peer", "approve", &client.id, "--config", &self.config];
        args.extend_from_slice(grant);
        run_ok(&args)
    }

    fn revoke(&self, client: &Client) {
        run_ok(&["peer", "revoke", &client.id, "--config", &self.config]);
    }

    fn peers(&self) -> String {
        run_ok(&["peer", "list", "--config", &self.config]).stdout
    }
}

/// Whether something accepts connections at `address`, refusing a socket that connected to itself
/// (a loopback connect to a closed port in the ephemeral range can).
fn reachable(address: SocketAddr) -> bool {
    let until = Instant::now() + DEADLINE;
    while Instant::now() < until {
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(2))
            && stream.local_addr().ok() != stream.peer_addr().ok()
        {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// `bohio remote` for `client` against `host`, trusting the keys `join` stored.
fn remote(client: &Client, host: &Host, action: &[&str]) -> Finished {
    let mut args = vec!["remote", &host.endpoint, "--config", &client.config];
    args.extend_from_slice(action);
    run(&args)
}

/// A library connection of `client` to `host`.
fn connect(client: &Client, host: &Host) -> Result<Connection, Error> {
    // The hosts a peer trusts are the `[peer.<id>]` tables `join` wrote into its configuration.
    let proof = Proof::builder(client.data.identity().expect("an identity"))
        .responders(Trust::pinned([host.id.parse().expect("a peer id")]))
        .build()
        .expect("a proof");
    Connection::connect_tcp(
        host.address,
        Duration::from_secs(10),
        &proof,
        &Settings::default(),
    )
}

/// A file with `size` bytes of a fixed pseudo-random sequence, and its SHA-256.
fn big_file(path: &Path, size: u64) -> String {
    const CHUNK: usize = 1024 * 1024;
    let mut hash = Sha256::new();
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).expect("a file"));
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut chunk = vec![0_u8; CHUNK];
    for _ in 0..size / CHUNK as u64 {
        for byte in &mut chunk {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state.to_le_bytes()[0];
        }
        hash.update(&chunk);
        file.write_all(&chunk).expect("the file is written");
    }
    format!("{:x}", hash.finalize())
}

/// An approved peer: paired, then granted `grant`.
fn approved(world: &World, name: &str, grant: &[&str]) -> Client {
    let client = world.client(name);
    world.pair(&client);
    world.approve(&client, grant);
    client
}

fn is_refusal(error: &Error) -> bool {
    matches!(error, Error::Transport(Failure::Permission))
}

#[test]
fn an_approved_peer_gets_info_runs_exec_and_reads_a_file() {
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("hello.txt"), "read over the link\n").unwrap();
    let host = world.serve("warn");
    let client = world.client("laptop");
    world.pair(&client);
    let root_text = root.to_string_lossy().into_owned();
    world.approve(
        &client,
        &["--allow", "info,exec,fs.read", "--fs-root", &root_text],
    );

    let info = remote(&client, &host, &["info", "--format", "json"]);
    assert_eq!(info.code, Some(0), "{}", info.stderr);
    assert!(
        info.stdout
            .contains(&format!("\"host_id\":\"{}\"", host.id)),
        "{}",
        info.stdout
    );

    let echo = remote(
        &client,
        &host,
        &["exec", "--", &helper(), "echo", "hello", "world"],
    );
    assert_eq!(echo.code, Some(0), "{}", echo.stderr);
    assert_eq!(echo.stdout.trim(), "hello world");
    let failing = remote(&client, &host, &["exec", "--", &helper(), "exit", "3"]);
    assert_eq!(
        failing.code,
        Some(3),
        "the program's status is the command's"
    );

    let path = root.join("hello.txt").to_string_lossy().into_owned();
    let read = remote(&client, &host, &["read", &path]);
    assert_eq!(read.code, Some(0), "{}", read.stderr);
    assert_eq!(read.stdout, "read over the link\n");

    // The peer holds no write permission, so a write through the library is refused at the open.
    let link = connect(&client, &host).expect("the link is up");
    let refused = Writer::start(
        &link,
        &WriteRequest::replace(&path),
        Some(Duration::from_secs(20)),
    );
    assert!(
        matches!(refused, Err(ref error) if is_refusal(error)),
        "{refused:?}"
    );
}

#[test]
fn an_unknown_proven_key_is_refused_and_listed_as_waiting() {
    let world = World::new("");
    let host = world.serve("warn");
    let stranger = world.client("stranger");

    // The stranger trusts the host by its id but the host has never heard of the stranger.
    let attempt = |action: &[&str]| {
        let mut args = vec![
            "remote",
            &host.endpoint,
            "--config",
            &stranger.config,
            "--server",
            &host.id,
        ];
        args.extend_from_slice(action);
        run(&args)
    };
    let refused = attempt(&["info"]);
    assert_ne!(
        refused.code,
        Some(0),
        "an unknown key was served: {}",
        refused.stdout
    );
    assert!(refused.stdout.is_empty(), "{}", refused.stdout);

    let listing = world.peers();
    assert!(
        listing.contains(&format!("waiting {}", stranger.id)),
        "the proven key is not listed as waiting:\n{listing}"
    );
    assert!(!listing.contains("accepted"), "{listing}");
    // Still refused: waiting is not approval.
    assert_ne!(attempt(&["info"]).code, Some(0));

    // The control: once approved, the same client with the same command is served.
    world.approve(&stranger, &["--allow", "info"]);
    let served = attempt(&["info"]);
    assert_eq!(served.code, Some(0), "{}", served.stderr);
    let listing = world.peers();
    assert!(
        listing.contains(&format!("accepted {}", stranger.id)),
        "{listing}"
    );
    assert!(!listing.contains("waiting"), "{listing}");
}

#[test]
fn an_approved_peer_without_exec_is_refused_it() {
    let world = World::new("");
    let host = world.serve("warn");
    let client = approved(&world, "reader", &["--allow", "info"]);

    let info = remote(&client, &host, &["info"]);
    assert_eq!(info.code, Some(0), "{}", info.stderr);

    let exec = remote(&client, &host, &["exec", "--", &helper(), "echo", "no"]);
    assert_ne!(exec.code, Some(0));
    assert!(exec.stdout.is_empty(), "{}", exec.stdout);
    assert!(exec.stderr.contains("refused"), "{}", exec.stderr);

    let link = connect(&client, &host).expect("the link is up");
    let started = Exec::start(&link, &Request::argv([helper().as_str(), "echo", "no"]))
        .expect("the stream opens")
        .wait(Some(Duration::from_secs(20)));
    assert!(
        matches!(started, Err(ref error) if is_refusal(error)),
        "{started:?}"
    );
    let shell = Shell::start(
        &link,
        &ShellRequest::kind(ShellKind::Sh),
        Some(Duration::from_secs(20)),
    );
    assert!(
        matches!(shell, Err(ref error) if is_refusal(error)),
        "{:?}",
        shell.err()
    );
}

/// Waits for `next` to fail, returning how long it took and the error; fails at the deadline.
fn ends_with_error<T>(mut next: impl FnMut() -> Result<T, Error>, what: &str) -> (Duration, Error) {
    let started = Instant::now();
    loop {
        match next() {
            Err(Error::Timeout) | Ok(_) => {}
            Err(error) => return (started.elapsed(), error),
        }
        assert!(
            started.elapsed() < DEADLINE,
            "{what} was not ended by the revoke within the deadline"
        );
    }
}

#[test]
fn a_revoke_ends_a_running_exec_within_recheck_and_its_process() {
    let world = World::new("");
    let host = world.serve("warn");
    let client = approved(&world, "ops", &["--allow", "exec"]);
    let link = connect(&client, &host).expect("the link is up");
    let exec = Exec::start(&link, &Request::argv([helper().as_str(), "tree", "120000"])).unwrap();
    let Ok(Some(ExecEvent::Started { pid })) = exec.next(Some(Duration::from_secs(20))) else {
        panic!("the process did not start");
    };
    assert!(alive(pid));

    world.revoke(&client);
    let (took, error) = ends_with_error(|| exec.next(Some(Duration::from_millis(100))), "the exec");
    eprintln!("exec ended {took:?} after the revoke (recheck {RECHECK:?})");
    assert!(
        is_refusal(&error),
        "the stream was not ended by the admission: {error:?}"
    );
    assert!(took <= RECHECK + SLACK, "took {took:?}");
    assert!(
        gone_within(pid, Duration::from_secs(15)),
        "the revoked peer's process is still running"
    );
}

#[test]
fn a_hand_edit_narrowing_the_user_layer_ends_a_running_exec_within_recheck() {
    let world = World::new("");
    let system_before = std::fs::read(&world.config).unwrap();
    let user = world.path("user.cfg");
    std::fs::write(&user, "# the user layer\n").unwrap();
    let user_text = user.to_string_lossy().into_owned();
    let layers = [
        "--config",
        world.config.as_str(),
        "--config",
        user_text.as_str(),
    ];
    let host = world.serve_layers(&layers, "warn");
    let client = world.client("ops");

    let mut door = Proc::start(
        &[
            "pair",
            "--config",
            &world.config,
            "--config",
            &user_text,
            "--timeout",
            "60s",
        ],
        None,
        "warn",
    );
    let endpoint = door.expect_line("pairing on ");
    let pin = door.expect_line("pin ");
    run_ok(&["join", &endpoint, "--pin", &pin, "--config", &client.config]);
    assert_eq!(door.finish().code, Some(0));
    let mut approve = vec!["peer", "approve", client.id.as_str(), "--allow", "exec"];
    approve.extend_from_slice(&layers);
    let approved = run_ok(&approve);
    assert!(approved.stdout.contains(&user_text), "{}", approved.stdout);
    assert_eq!(
        std::fs::read(&world.config).unwrap(),
        system_before,
        "the system layer is untouched: the peer lives in the last layer that can be written"
    );

    let link = connect(&client, &host).expect("the link is up");
    let exec = Exec::start(&link, &Request::argv([helper().as_str(), "tree", "120000"])).unwrap();
    let Ok(Some(ExecEvent::Started { pid })) = exec.next(Some(Duration::from_secs(20))) else {
        panic!("the process did not start");
    };
    assert!(alive(pid));

    // A person edits the user file by hand: exec is no longer allowed.
    let text = std::fs::read_to_string(&user).unwrap();
    assert!(text.contains("allow = [\"exec\"]"), "{text}");
    std::fs::write(
        &user,
        text.replace("allow = [\"exec\"]", "allow = [\"info\"]"),
    )
    .unwrap();
    let (took, error) = ends_with_error(|| exec.next(Some(Duration::from_millis(100))), "the exec");
    assert!(is_refusal(&error), "{error:?}");
    assert!(took <= RECHECK + SLACK, "took {took:?}");
    assert!(gone_within(pid, Duration::from_secs(15)));
}

#[test]
fn narrowing_a_grant_ends_the_streams_it_no_longer_covers_while_the_peer_stays_accepted() {
    // Only the admission can end this stream: the key stays accepted, so neither the trust store nor
    // the link's own check changes.
    let world = World::new("");
    let host = world.serve("warn");
    let client = approved(&world, "ops", &["--allow", "info,exec"]);
    let link = connect(&client, &host).expect("the link is up");
    let exec = Exec::start(
        &link,
        &Request::argv([helper().as_str(), "sleep", "120000"]),
    )
    .unwrap();
    let Ok(Some(ExecEvent::Started { pid })) = exec.next(Some(Duration::from_secs(20))) else {
        panic!("the process did not start");
    };

    world.approve(&client, &["--allow", "info"]);
    let (took, error) = ends_with_error(|| exec.next(Some(Duration::from_millis(100))), "the exec");
    eprintln!("exec ended {took:?} after the grant was narrowed (recheck {RECHECK:?})");
    assert!(is_refusal(&error), "{error:?}");
    assert!(took <= RECHECK + SLACK, "took {took:?}");
    assert!(gone_within(pid, Duration::from_secs(15)));

    // The peer is still accepted: its link is up and what it may still do works.
    let control = bohio_client::control::Control::open(&link).expect("the control stream opens");
    assert!(control.info(Some(Duration::from_secs(20))).is_ok());
    assert!(world.peers().contains(&format!("accepted {}", client.id)));
}

#[test]
fn a_control_stream_already_open_follows_a_narrowed_grant_on_its_next_call() {
    // The stream keeps a permission its admission accepts (fs.read), so only a per-call decision can
    // take away fs.write or a root while it stays open.
    let world = World::new("");
    let root = world.path("files");
    let other = world.path("other");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let root_text = root.to_string_lossy().into_owned();
    let other_text = other.to_string_lossy().into_owned();
    let host = world.serve("warn");
    let client = approved(
        &world,
        "editor",
        &["--allow", "info,fs.read,fs.write", "--fs-root", &root_text],
    );
    let link = connect(&client, &host).expect("the link is up");
    let files = bohio_client::fs::Fs::connect(&link)
        .expect("the control stream opens")
        .with_timeout(Some(DEADLINE));
    let before = format!("{root_text}/before");
    files
        .mkdir(&before, false)
        .expect("fs.write makes a directory");

    world.approve(
        &client,
        &["--allow", "info,fs.read", "--fs-root", &root_text],
    );
    let refused = files.mkdir(&format!("{root_text}/after"), false);
    assert!(
        matches!(&refused, Err(Error::Remote(body)) if body.code == bohio_proto::ErrorCode::Denied),
        "a peer that lost fs.write still made a directory: {refused:?}"
    );
    assert!(!root.join("after").exists());
    assert!(files.stat(&before, false).is_ok(), "fs.read still holds");

    world.approve(
        &client,
        &["--allow", "info,fs.read", "--fs-root", &other_text],
    );
    let moved = files.stat(&before, false);
    assert!(
        matches!(&moved, Err(Error::Remote(body)) if body.code == bohio_proto::ErrorCode::OutsideRoot),
        "a root taken away is still served: {moved:?}"
    );
}

#[test]
fn narrowing_a_program_list_ends_a_running_exec_of_a_program_it_no_longer_lists() {
    let world = World::new("");
    let host = world.serve("warn");
    let program = helper();
    let client = approved(
        &world,
        "runner",
        &["--allow", "exec", "--program", &program],
    );
    let link = connect(&client, &host).expect("the link is up");
    let exec = Exec::start(&link, &Request::argv([program.as_str(), "sleep", "120000"])).unwrap();
    let Ok(Some(ExecEvent::Started { pid })) = exec.next(Some(Duration::from_secs(20))) else {
        panic!("the process did not start");
    };

    world.approve(
        &client,
        &["--allow", "exec", "--program", "some-other-program"],
    );
    let (took, error) = ends_with_error(|| exec.next(Some(Duration::from_millis(100))), "the exec");
    eprintln!("exec ended {took:?} after its program left the list (recheck {RECHECK:?})");
    assert!(is_refusal(&error), "{error:?}");
    assert!(took <= RECHECK + SLACK, "took {took:?}");
    assert!(gone_within(pid, Duration::from_secs(15)));
}

/// Whether the host has closed `stream`: a read sees the end or a reset within `within`, rather than
/// waiting, as it does while the host waits for the peer's proof.
fn closed_by_host(stream: &mut TcpStream, within: Duration) -> bool {
    stream.set_read_timeout(Some(within)).unwrap();
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    }
}

#[test]
fn one_source_cannot_hold_every_connection_while_proving_nothing() {
    let world = World::new("max_unproven_per_source = 2\nproof_timeout = \"30s\"");
    let host = world.serve("warn");
    // The host's own reachability probe is a connection too: let it be seen to end.
    thread::sleep(Duration::from_secs(1));
    let mut idle: Vec<TcpStream> = (0..2)
        .map(|_| TcpStream::connect(host.address).expect("a connection"))
        .collect();
    for stream in &mut idle {
        assert!(
            !closed_by_host(stream, Duration::from_millis(500)),
            "a connection within the bound was closed"
        );
    }
    let mut third = TcpStream::connect(host.address).expect("a connection");
    assert!(
        closed_by_host(&mut third, Duration::from_secs(5)),
        "a third connection from one source was held while two were proving nothing"
    );

    // Once they go, the source may connect again, and a proven link is not counted against it.
    drop(idle);
    let client = approved(&world, "late", &["--allow", "info"]);
    let until = Instant::now() + DEADLINE;
    let link = loop {
        match connect(&client, &host) {
            Ok(link) => break link,
            Err(error) if Instant::now() < until => {
                eprintln!("retrying: {error}");
                thread::sleep(Duration::from_millis(200));
            }
            Err(error) => panic!("{error:?}"),
        }
    };
    let _more: Vec<TcpStream> = (0..2)
        .map(|_| TcpStream::connect(host.address).expect("a connection"))
        .collect();
    let control = bohio_client::control::Control::open(&link).expect("the control stream opens");
    assert!(control.info(Some(DEADLINE)).is_ok());
}

#[test]
fn one_peer_cannot_hold_more_links_than_max_links_per_peer() {
    let world = World::new("max_links_per_peer = 1");
    let host = world.serve("warn");
    let client = approved(&world, "greedy", &["--allow", "info"]);
    let first = connect(&client, &host).expect("the first link is up");
    let info = |link: &Connection| {
        bohio_client::control::Control::open(link)
            .and_then(|control| control.info(Some(Duration::from_secs(10))))
    };
    assert!(info(&first).is_ok());
    let second = connect(&client, &host).and_then(|link| info(&link).map(|_| ()));
    assert!(
        second.is_err(),
        "a second link was served past max_links_per_peer = 1"
    );
    assert!(info(&first).is_ok(), "the first link still works");

    // The link given back makes room for another.
    drop(first);
    let until = Instant::now() + DEADLINE;
    loop {
        let again = connect(&client, &host).and_then(|link| info(&link).map(|_| ()));
        if again.is_ok() {
            break;
        }
        assert!(
            Instant::now() < until,
            "no link after the first ended: {again:?}"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_revoke_ends_a_running_shell_within_recheck_and_its_process() {
    let world = World::new("");
    let host = world.serve("warn");
    let client = approved(&world, "ops", &["--allow", "shell"]);
    let link = connect(&client, &host).expect("the link is up");
    #[cfg(unix)]
    let kind = ShellKind::Sh;
    #[cfg(windows)]
    let kind = ShellKind::Cmd;
    let shell = Shell::start(
        &link,
        &ShellRequest::kind(kind),
        Some(Duration::from_secs(40)),
    )
    .expect("the shell starts");
    let pid = shell.pid();
    assert!(alive(pid));

    world.revoke(&client);
    let (took, error) = ends_with_error(
        || match shell.next(Some(Duration::from_millis(100))) {
            Ok(Some(ShellEvent::Exit(_))) | Ok(None) => Err(Error::Closed),
            other => other.map(|_| ()),
        },
        "the shell",
    );
    eprintln!("shell ended {took:?} after the revoke (recheck {RECHECK:?})");
    assert!(
        is_refusal(&error),
        "the stream was not ended by the admission: {error:?}"
    );
    assert!(took <= RECHECK + SLACK, "took {took:?}");
    assert!(
        gone_within(pid, Duration::from_secs(15)),
        "the revoked peer's shell is still running"
    );
}

#[test]
fn a_revoke_ends_running_transfers_within_recheck_and_leaves_no_partial_file() {
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    big_file(&root.join("big.bin"), 32 * 1024 * 1024);
    let host = world.serve("warn");
    let root_text = root.to_string_lossy().into_owned();
    let client = approved(
        &world,
        "mover",
        &["--allow", "fs.read,fs.write", "--fs-root", &root_text],
    );
    let link = connect(&client, &host).expect("the link is up");
    let wait = Some(Duration::from_secs(20));
    let reader = Reader::start(&link, &ReadRequest::whole("big.bin"), wait).unwrap();
    assert!(reader.next_chunk(wait).unwrap().is_some());
    let writer = Writer::start(&link, &WriteRequest::replace("upload.bin"), wait).unwrap();
    writer.write(&[7_u8; 64 * 1024]).unwrap();

    world.revoke(&client);
    let mut read = 0_u64;
    let (read_took, read_error) = ends_with_error(
        || {
            let chunk = reader.next_chunk(Some(Duration::from_millis(100)))?;
            read += chunk.map_or(0, |bytes| bytes.len() as u64);
            Ok(())
        },
        "the read",
    );
    assert!(is_refusal(&read_error), "{read_error:?}");
    assert!(
        read < 32 * 1024 * 1024,
        "the whole file was sent after the revoke"
    );
    let (write_took, write_error) =
        ends_with_error(|| writer.write(&[7_u8; 64 * 1024]), "the write");
    eprintln!(
        "read ended {read_took:?}, write {write_took:?} after the revoke (recheck {RECHECK:?})"
    );
    assert!(is_refusal(&write_error), "{write_error:?}");
    assert!(read_took <= RECHECK + SLACK && write_took <= RECHECK + SLACK);

    // The abandoned upload put nothing in place and left no temporary file behind.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        if names == ["big.bin"] {
            break;
        }
        assert!(Instant::now() < deadline, "the root holds {names:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// The resident memory of process `pid`, in bytes.
fn resident(system: &mut System, pid: u32) -> u64 {
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map_or(0, sysinfo::Process::memory)
}

/// The most the process `pid` grew above `baseline` while `work` ran.
fn peak_growth(pid: u32, baseline: u64, work: impl FnOnce()) -> u64 {
    struct Stop<'a>(&'a AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let sampler = scope.spawn(|| {
            let mut system = System::new();
            let mut peak = baseline;
            while !stop.load(Ordering::Acquire) {
                peak = peak.max(resident(&mut system, pid));
                thread::sleep(Duration::from_millis(5));
            }
            peak.max(resident(&mut system, pid))
        });
        {
            let _stop = Stop(&stop);
            work();
        }
        sampler.join().unwrap().saturating_sub(baseline)
    })
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS CI: the resident set did not grow when the host held the whole file (the control test failed), so it cannot show a bound there"
)]
fn a_64_mib_file_crosses_the_real_link_both_ways_with_bounded_host_memory() {
    const SIZE: u64 = 64 * 1024 * 1024;
    const BOUND: u64 = 16 * 1024 * 1024;
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    let want = big_file(&root.join("big.bin"), SIZE);
    let host = world.serve("warn");
    let root_text = root.to_string_lossy().into_owned();
    let client = approved(
        &world,
        "mover",
        &["--allow", "fs.read,fs.write", "--fs-root", &root_text],
    );
    let link = connect(&client, &host).expect("the link is up");
    let wait = Some(Duration::from_secs(60));

    // One small exchange first, so the threads and buffers the host always needs are not counted.
    {
        let writer = Writer::start(&link, &WriteRequest::replace("warm.txt"), wait).unwrap();
        writer.write(b"warm").unwrap();
        writer.finish(wait).unwrap();
        let again = Reader::start(&link, &ReadRequest::whole("warm.txt"), wait).unwrap();
        assert_eq!(again.read_to_end(wait).unwrap(), b"warm");
    }
    let host_pid = host.process.id();
    let baseline = resident(&mut System::new(), host_pid);
    assert!(baseline > 0, "the host's memory cannot be read");

    let mut hash = Sha256::new();
    let mut total = 0_u64;
    let read_growth = peak_growth(host_pid, baseline, || {
        let reader = Reader::start(&link, &ReadRequest::whole("big.bin"), wait).unwrap();
        assert_eq!(reader.header().length, SIZE);
        while let Some(chunk) = reader.next_chunk(wait).unwrap() {
            hash.update(&chunk);
            total += chunk.len() as u64;
        }
    });
    assert_eq!(
        (total, format!("{:x}", hash.finalize())),
        (SIZE, want.clone())
    );

    let write_growth = peak_growth(host_pid, baseline, || {
        let writer = Writer::start(&link, &WriteRequest::replace("copy.bin"), wait).unwrap();
        let mut file = std::fs::File::open(root.join("big.bin")).unwrap();
        let mut chunk = vec![0_u8; 1024 * 1024];
        loop {
            let count = file.read(&mut chunk).unwrap();
            if count == 0 {
                break;
            }
            writer.write(&chunk[..count]).unwrap();
        }
        let done = writer.finish(wait).unwrap();
        assert_eq!((done.written, done.sha256.as_str()), (SIZE, want.as_str()));
    });
    eprintln!(
        "64 MiB over the link: host grew {read_growth} bytes reading, {write_growth} writing"
    );
    assert!(
        read_growth < BOUND,
        "reading grew the host by {read_growth} bytes"
    );
    assert!(
        write_growth < BOUND,
        "writing grew the host by {write_growth} bytes"
    );
    assert_eq!(
        std::fs::metadata(root.join("copy.bin")).unwrap().len(),
        SIZE
    );
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS CI: the resident set did not grow when the host held the whole file (the control test failed), so it cannot show a bound there"
)]
fn the_link_settings_are_what_bound_the_hosts_memory() {
    // The control for the bound above: with a stream's queue made as large as the file, a reader
    // that takes nothing makes the host hold the file, so the bound is the settings and not luck.
    const SIZE: u64 = 64 * 1024 * 1024;
    let world = World::new("queue_bytes = 268435456\n");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    big_file(&root.join("big.bin"), SIZE);
    let host = world.serve("warn");
    let root_text = root.to_string_lossy().into_owned();
    let client = approved(
        &world,
        "slow",
        &["--allow", "fs.read", "--fs-root", &root_text],
    );
    let link = connect(&client, &host).expect("the link is up");
    let wait = Some(Duration::from_secs(60));
    {
        let warm = Reader::start(&link, &ReadRequest::whole("big.bin"), wait).unwrap();
        drop(warm);
    }
    thread::sleep(Duration::from_secs(2));
    let host_pid = host.process.id();
    let mut system = System::new();
    let baseline = resident(&mut system, host_pid);
    let reader = Reader::start(&link, &ReadRequest::whole("big.bin"), wait).unwrap();
    let until = Instant::now() + Duration::from_secs(40);
    let mut peak = 0;
    while Instant::now() < until && peak < 16 * 1024 * 1024 {
        peak = resident(&mut system, host_pid).saturating_sub(baseline);
        thread::sleep(Duration::from_millis(50));
    }
    drop(reader);
    assert!(
        peak >= 16 * 1024 * 1024,
        "an unbounded queue did not make the host hold the file (grew {peak} bytes)"
    );
}

#[test]
fn a_shell_session_runs_a_command_through_the_command_line() {
    let world = World::new("");
    let host = world.serve("warn");
    let client = approved(&world, "ops", &["--allow", "shell"]);
    #[cfg(unix)]
    let (kind, script) = ("sh", "echo $((20 + 22))\rexit\r");
    #[cfg(windows)]
    let (kind, script) = ("cmd", "set /a 20 + 22\rexit\r");
    let process = Proc::start(
        &[
            "remote",
            &host.endpoint,
            "--config",
            &client.config,
            "shell",
            "--kind",
            kind,
        ],
        Some(script),
        "warn",
    );
    let done = process.finish();
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    // The terminal echoes what was typed, which holds `20 + 22`; only a shell that ran it prints 42 alone.
    assert!(
        done.stdout
            .lines()
            .any(|line| line.split('\r').any(|part| part.trim() == "42")),
        "{}",
        done.stdout
    );
}

/// Every run of the pairing and the session below logs at `trace`; none of it may hold a secret.
#[test]
fn the_log_of_a_pairing_and_a_session_holds_no_key_pin_or_secret() {
    let world = World::new("");
    let host = world.serve("trace");
    let client = world.client("secretive");

    let mut door = Proc::start(
        &["pair", "--config", &world.config, "--timeout", "60s"],
        None,
        "trace",
    );
    let endpoint = door.expect_line("pairing on ");
    let pin = door.expect_line("pin ");
    let mut logs = Vec::new();
    let join = Proc::start(
        &["join", &endpoint, "--pin", &pin, "--config", &client.config],
        None,
        "trace",
    );
    let joined = join.finish();
    assert_eq!(joined.code, Some(0), "{}", joined.stderr);
    logs.push(joined.stderr);
    let door = door.finish();
    assert_eq!(door.code, Some(0), "{}", door.stderr);
    logs.push(door.stderr);

    world.approve(&client, &["--allow", "info,exec"]);
    let run_logged = |action: &[&str]| {
        let mut args = vec!["remote", &host.endpoint, "--config", &client.config];
        args.extend_from_slice(action);
        Proc::start(&args, None, "trace").finish()
    };
    logs.push(run_logged(&["info"]).stderr);
    logs.push(run_logged(&["exec", "--", &helper(), "echo", "session"]).stderr);

    // Let the host log the end of the session, then take its whole log.
    thread::sleep(Duration::from_secs(1));
    let host_log = text(&host.process.stderr);
    assert!(
        host_log.contains("link up"),
        "the host's log is empty, so this test proves nothing:\n{host_log}"
    );
    logs.push(host_log);
    let log = logs.join("\n");

    // What must never appear: the PIN, and any of the keys' secret material as it is stored.
    let mut secrets = Vec::new();
    // A PIN is six digits, so it is looked for as a number on its own, not inside a timestamp or an id.
    assert!(!holds_number(&log, &pin), "the log holds the PIN");
    for (who, dir) in [
        ("the host's key", world.path("host-data")),
        ("the client's key", world.path("secretive-data")),
    ] {
        let stored = std::fs::read_to_string(dir.join("identity")).expect("an identity file");
        for line in stored
            .lines()
            .filter(|line| line.len() >= 16 && !line.starts_with("-----"))
        {
            secrets.push((format!("a line of {who}"), line.trim().to_owned()));
        }
        let block = coqui_peer::pem::decode(&stored).expect("a PEM block");
        for window in block.bytes.chunks(8).filter(|part| part.len() == 8) {
            let hex: String = window.iter().map(|byte| format!("{byte:02x}")).collect();
            secrets.push((format!("bytes of {who} in hex"), hex));
        }
    }
    for (what, secret) in &secrets {
        assert!(!log.contains(secret.as_str()), "the log holds {what}");
    }
    assert!(secrets.len() > 4, "the secrets to look for were not found");
}

/// Whether `log` holds `number` as a run of digits of its own.
fn holds_number(log: &str, number: &str) -> bool {
    let bytes = log.as_bytes();
    log.match_indices(number).any(|(at, found)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + found.len()).copied();
        !before.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'.')
            && !after.is_some_and(|b| b.is_ascii_alphanumeric())
    })
}
