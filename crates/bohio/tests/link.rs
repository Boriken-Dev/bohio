//! The authenticated link end to end: `bohio serve` runs as a child process on `127.0.0.1:0`, peers pair
//! and prove themselves to it, and the host decides what each may do from the `[peer.<id>]` tables of its
//! `bohio.cfg` layers.
//!
//! Every wait has a deadline, every child is killed when its handle drops, and every data directory is
//! under the system temporary directory.

#![forbid(unsafe_code)]

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
use bohio_proto::hash::Algorithm;
use bohio_proto::shell::{Request as ShellRequest, ShellKind};
use bohio_transfer::hash::Hasher;
use cabuya::auth::{Proof, Trust};
use coqui_transport::Failure;
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
    /// The input pipe, kept open until the process drops, when the test asked for that.
    _kept: Option<std::process::ChildStdin>,
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
        Self::start_with(args, stdin, false, log)
    }

    /// As [`Proc::start`]; with `keep_open` the input stays open after `stdin` is written.
    fn start_with(args: &[&str], stdin: Option<&str>, keep_open: bool, log: &str) -> Self {
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
        let mut kept = None;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input.as_bytes()).expect("input is written");
            if keep_open {
                kept = Some(pipe);
            }
        }
        let (stdout, out_reader) = collect(child.stdout.take().unwrap());
        let (stderr, err_reader) = collect(child.stderr.take().unwrap());
        Self {
            child,
            _kept: kept,
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

    /// Kills the process (SIGKILL on Unix) and collects it.
    #[cfg(unix)]
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    /// Where the host started last listens: pairing is armed on it, with no port of its own.
    serving: Mutex<Option<String>>,
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
        let dir = crate::support::private_dir::private_tempdir();
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
            serving: Mutex::new(None),
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
        *self.serving.lock().unwrap() = Some(endpoint.clone());
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

    /// Pairs `client` with the running host by a PIN: `bohio pair` arms it on the host, and the client joins on the
    /// host's own listener with it.
    fn pair(&self, client: &Client) {
        let endpoint = self
            .serving
            .lock()
            .unwrap()
            .clone()
            .expect("a host is serving");
        let mut door = Proc::start(
            &["pair", "--config", &self.config, "--timeout", "60s"],
            None,
            "warn",
        );
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
    connect_with(client, host, &Settings::default())
}

fn connect_with(client: &Client, host: &Host, settings: &Settings) -> Result<Connection, Error> {
    // The hosts a peer trusts are the `[peer.<id>]` tables `join` wrote into its configuration.
    let proof = Proof::builder(client.data.identity().expect("an identity"))
        .responders(Trust::pinned([host.id.parse().expect("a peer id")]))
        .build()
        .expect("a proof");
    Connection::connect_tcp(host.address, Duration::from_secs(10), &proof, settings)
}

/// A file with `size` bytes of a fixed pseudo-random sequence, and its SHA-256.
fn big_file(path: &Path, size: u64) -> String {
    const CHUNK: usize = 1024 * 1024;
    let mut hash = Hasher::new(&[Algorithm::Sha256]).unwrap();
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
    hash.finish()[&Algorithm::Sha256].clone()
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

    // The same bytes hashed on the host: one algorithm prints `HEX  PATH`, several `NAME HEX  PATH`, a missing file is
    // reported and ends the command with status 1 after the rest are printed.
    let mut local = Hasher::new(&[Algorithm::Sha256, Algorithm::Crc32c]).unwrap();
    local.update(b"read over the link\n");
    let local = local.finish();
    let one = remote(&client, &host, &["hash", &path]);
    assert_eq!(one.code, Some(0), "{}", one.stderr);
    assert_eq!(
        one.stdout.trim(),
        format!("{}  {path}", local[&Algorithm::Sha256])
    );
    let missing = root.join("absent.txt").to_string_lossy().into_owned();
    let two = remote(
        &client,
        &host,
        &[
            "hash",
            "--algorithm",
            "sha256",
            "--algorithm",
            "crc32c",
            &missing,
            &path,
        ],
    );
    assert_eq!(two.code, Some(1), "{}", two.stderr);
    assert_eq!(
        two.stdout.lines().collect::<Vec<_>>(),
        [
            format!("sha256 {}  {path}", local[&Algorithm::Sha256]),
            format!("crc32c {}  {path}", local[&Algorithm::Crc32c]),
        ]
    );
    assert!(two.stderr.contains("absent.txt"), "{}", two.stderr);
    let unknown = remote(&client, &host, &["hash", "--algorithm", "whirlpool", &path]);
    assert_eq!(unknown.code, Some(1));

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
fn the_command_reads_writes_and_runs_programs_compressed() {
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    let text: String = (0..20_000)
        .map(|n| {
            format!(
                "line {n}: the quick brown fox {} jumps over the lazy dog\n",
                n % 17
            )
        })
        .collect();
    std::fs::write(root.join("log.txt"), &text).unwrap();
    let host = world.serve("warn");
    let client = approved(
        &world,
        "squeezer",
        &[
            "--allow",
            "info,exec,fs.read,fs.write",
            "--fs-root",
            &root.to_string_lossy(),
        ],
    );
    let path = root.join("log.txt").to_string_lossy().into_owned();

    // What the host offers is printed with its levels.
    let info = remote(&client, &host, &["info"]);
    assert_eq!(info.code, Some(0), "{}", info.stderr);
    assert!(
        info.stdout.contains("compression: lz4 no levels"),
        "{}",
        info.stdout
    );
    assert!(
        info.stdout
            .contains("compression: deflate levels 0..9 (default 6)"),
        "{}",
        info.stdout
    );
    assert!(
        info.stdout.contains("prepared reads: enabled"),
        "{}",
        info.stdout
    );

    // Reads: each algorithm, and prepared, give the file's own bytes.
    let mut algorithms = vec!["lz4", "deflate:9", "auto", "none"];
    if cfg!(feature = "zstd") {
        algorithms.extend(["zstd:3", "zstd:19"]);
    }
    for algorithm in algorithms {
        let read = remote(&client, &host, &["read", "--compress", algorithm, &path]);
        assert_eq!(read.code, Some(0), "{algorithm}: {}", read.stderr);
        assert_eq!(read.stdout, text, "{algorithm}");
    }
    let prepared = remote(
        &client,
        &host,
        &["read", "--compress", "deflate", "--prepare", &path],
    );
    assert_eq!(prepared.code, Some(0), "{}", prepared.stderr);
    assert_eq!(prepared.stdout, text);
    let unknown = remote(&client, &host, &["read", "--compress", "brotli", &path]);
    assert_eq!(unknown.code, Some(1));
    assert!(
        unknown.stderr.contains("not an algorithm"),
        "{}",
        unknown.stderr
    );
    let alone = remote(&client, &host, &["read", "--prepare", &path]);
    assert_eq!(alone.code, Some(2), "--prepare needs --compress");

    // Writes: standard input, compressed, lands intact.
    for (index, algorithm) in ["lz4", "deflate:6", "auto"].into_iter().enumerate() {
        let target = root
            .join(format!("copy{index}.txt"))
            .to_string_lossy()
            .into_owned();
        let mut args = vec!["remote", &host.endpoint, "--config", &client.config];
        args.extend(["write", "--compress", algorithm, &target]);
        let done = Proc::start(&args, Some(&text), "warn").finish();
        assert_eq!(done.code, Some(0), "{algorithm}: {}", done.stderr);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            text,
            "{algorithm}"
        );
    }
    let target = root.join("prepared.txt").to_string_lossy().into_owned();
    let mut args = vec!["remote", &host.endpoint, "--config", &client.config];
    args.extend(["write", "--compress", "deflate", "--prepare", &target]);
    let done = Proc::start(&args, Some(&text), "warn").finish();
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), text);

    // Programs: output and input.
    let both = remote(
        &client,
        &host,
        &[
            "exec",
            "--compress",
            "lz4",
            "--",
            &helper(),
            "text",
            "50000",
        ],
    );
    assert_eq!(both.code, Some(0), "{}", both.stderr);
    assert_eq!(both.stdout.len(), 50_000);
    assert_eq!(both.stderr.len(), 50_000);
    let program = helper();
    let mut args = vec!["remote", &host.endpoint, "--config", &client.config];
    args.extend([
        "exec",
        "--stdin",
        "--compress",
        "deflate",
        "--",
        &program,
        "cat",
    ]);
    let echoed = Proc::start(&args, Some(&text), "warn").finish();
    assert_eq!(echoed.code, Some(0), "{}", echoed.stderr);
    assert_eq!(echoed.stdout, text);
}

/// The SHA-256 of a file's bytes.
fn sha256_of(path: &Path) -> String {
    let mut hash = Hasher::new(&[Algorithm::Sha256]).unwrap();
    hash.update(&std::fs::read(path).expect("the file reads"));
    hash.finish()[&Algorithm::Sha256].clone()
}

/// The numbers of `sent W of S (copied C, literal L)`.
fn sent_line(output: &str) -> (u64, u64, u64, u64) {
    let line = output.trim();
    let numbers: Vec<u64> = line
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|word| word.parse().ok())
        .collect();
    assert!(line.starts_with("sent "), "{line}");
    (numbers[0], numbers[1], numbers[2], numbers[3])
}

#[test]
fn the_command_puts_and_gets_a_file_sending_only_what_changed() {
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    // A peer with no `info`: what the host offers comes from the `hello` reply.
    let host = world.serve("warn");
    let client = approved(
        &world,
        "syncer",
        &[
            "--allow",
            "fs.read,fs.write",
            "--fs-root",
            &root.to_string_lossy(),
        ],
    );
    let size: u64 = 3 << 20;
    let local = world.path("local.bin");
    let theirs = root.join("remote.bin");
    let remote_text = theirs.to_string_lossy().into_owned();
    let local_text = local.to_string_lossy().into_owned();
    big_file(&local, size);

    // No copy on the host: the whole file goes.
    let first = remote(&client, &host, &["put", &local_text, &remote_text]);
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    let (wire, total, copied, literal) = sent_line(&first.stdout);
    assert_eq!(
        (total, copied, literal),
        (size, 0, size),
        "{}",
        first.stdout
    );
    assert!(wire >= size);
    assert_eq!(sha256_of(&theirs), sha256_of(&local));

    // One byte changed: a delta moves a block and the signature, not the file.
    let mut bytes = std::fs::read(&local).unwrap();
    bytes[1_500_000] ^= 0x5a;
    std::fs::write(&local, &bytes).unwrap();
    let again = remote(&client, &host, &["put", &local_text, &remote_text]);
    assert_eq!(again.code, Some(0), "{}", again.stderr);
    let (wire, total, copied, literal) = sent_line(&again.stdout);
    assert_eq!(total, size);
    assert!(wire * 20 < size, "{}", again.stdout);
    assert!(
        literal <= 4096 && copied + literal == size,
        "{}",
        again.stdout
    );
    assert_eq!(sha256_of(&theirs), sha256_of(&local));

    // The same in the other direction.
    let mut bytes = std::fs::read(&theirs).unwrap();
    bytes[2_000_000] ^= 0xa5;
    std::fs::write(&theirs, &bytes).unwrap();
    let pulled = remote(&client, &host, &["get", &remote_text, &local_text]);
    assert_eq!(pulled.code, Some(0), "{}", pulled.stderr);
    let (wire, total, _, literal) = sent_line(&pulled.stdout);
    assert_eq!(total, size);
    assert!(wire * 20 < size && literal <= 4096, "{}", pulled.stdout);
    assert_eq!(sha256_of(&local), sha256_of(&theirs));

    // Two identical files with --compare: nothing moves; --delta never moves it all.
    let same = remote(
        &client,
        &host,
        &["get", "--compare", "hash", &remote_text, &local_text],
    );
    assert_eq!(same.code, Some(0), "{}", same.stderr);
    assert!(same.stdout.contains("(unchanged)"), "{}", same.stdout);
    let whole = remote(
        &client,
        &host,
        &[
            "get",
            "--delta",
            "never",
            "--compress",
            "lz4",
            &remote_text,
            &local_text,
        ],
    );
    assert_eq!(whole.code, Some(0), "{}", whole.stderr);
    let (_, _, copied, literal) = sent_line(&whole.stdout);
    assert_eq!((copied, literal), (0, size), "{}", whole.stdout);
    assert_eq!(sha256_of(&local), sha256_of(&theirs));
    // No temporary file stays behind on either side.
    let strays = |dir: &Path, keep: &str| {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".bohio-") || name.ends_with(".tmp"))
            .filter(|name| name != keep)
            .count()
    };
    assert_eq!(strays(&root, ""), 0);
    assert_eq!(strays(&world.path(""), ""), 0);
}

#[test]
fn the_command_moves_one_file_on_several_streams_and_links() {
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    let host = world.serve("warn");
    let client = approved(
        &world,
        "spreader",
        &[
            "--allow",
            "fs.read,fs.write",
            "--fs-root",
            &root.to_string_lossy(),
        ],
    );
    let size: u64 = 3 << 20;
    let local = world.path("local.bin");
    let theirs = root.join("remote.bin");
    let remote_text = theirs.to_string_lossy().into_owned();
    let local_text = local.to_string_lossy().into_owned();
    big_file(&local, size);

    // A put on four streams over one link, then on four streams over two links.
    let put = remote(
        &client,
        &host,
        &[
            "put",
            "--parallel",
            "4",
            "--range",
            "700000",
            &local_text,
            &remote_text,
        ],
    );
    assert_eq!(put.code, Some(0), "{}", put.stderr);
    assert_eq!(sha256_of(&theirs), sha256_of(&local));
    std::fs::remove_file(&theirs).unwrap();
    let put = remote(
        &client,
        &host,
        &[
            "put",
            "--parallel",
            "4",
            "--links",
            "2",
            &local_text,
            &remote_text,
        ],
    );
    assert_eq!(put.code, Some(0), "{}", put.stderr);
    assert_eq!(sha256_of(&theirs), sha256_of(&local));

    // A get over two links replaces the local file.
    let pulled = world.path("pulled.bin");
    let pulled_text = pulled.to_string_lossy().into_owned();
    let got = remote(
        &client,
        &host,
        &["get", "--links", "2", &remote_text, &pulled_text],
    );
    assert_eq!(got.code, Some(0), "{}", got.stderr);
    assert_eq!(sha256_of(&pulled), sha256_of(&local));

    // A read hands the ranges to standard output in order.
    let text: String = (0..60_000)
        .map(|line| format!("line {line:06}\n"))
        .collect();
    std::fs::write(root.join("text.txt"), &text).unwrap();
    let read = remote(
        &client,
        &host,
        &[
            "read",
            "--parallel",
            "3",
            "--range",
            "100000",
            &root.join("text.txt").to_string_lossy(),
        ],
    );
    assert_eq!(read.code, Some(0), "{}", read.stderr);
    assert!(read.stdout == text, "the output differs");

    // A delta or a comparison needs the ordinary transfer.
    let refused = remote(
        &client,
        &host,
        &[
            "get",
            "--parallel",
            "2",
            "--delta",
            "always",
            &remote_text,
            &pulled_text,
        ],
    );
    assert_ne!(refused.code, Some(0));
    assert!(refused.stderr.contains("--parallel"), "{}", refused.stderr);
    // Nothing is left beside either file.
    for dir in [&root, pulled.parent().unwrap()] {
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.contains(".part") && !name.contains("bohio-part"),
                "{name}"
            );
        }
    }
}

#[test]
fn the_command_walks_a_tree_and_syncs_it_both_ways_moving_only_what_differs() {
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    let host = world.serve("warn");
    let client = approved(
        &world,
        "treeman",
        &[
            "--allow",
            "fs.read,fs.write",
            "--fs-root",
            &root.to_string_lossy(),
        ],
    );
    let local = world.path("src");
    for dir in 0..3 {
        for file in 0..10 {
            let path = local.join(format!("d{dir}/f{file}.txt"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("file {dir}/{file}\n")).unwrap();
        }
    }
    big_file(&local.join("big.bin"), 2 << 20);
    let local_text = local.to_string_lossy().into_owned();
    let remote_text = root.join("tree").to_string_lossy().into_owned();

    let first = remote(&client, &host, &["sync", "push", &local_text, &remote_text]);
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert!(
        first
            .stdout
            .contains("0 unchanged, 30 bundled in 1 bundles, 1 large"),
        "{}",
        first.stdout
    );
    assert_eq!(
        sha256_of(&root.join("tree/big.bin")),
        sha256_of(&local.join("big.bin"))
    );
    assert_eq!(
        std::fs::read_to_string(root.join("tree/d2/f9.txt")).unwrap(),
        "file 2/9\n"
    );

    // The same again: size and time agree, nothing moves.
    let again = remote(&client, &host, &["sync", "push", &local_text, &remote_text]);
    assert_eq!(again.code, Some(0), "{}", again.stderr);
    assert!(
        again
            .stdout
            .contains("31 unchanged, 0 bundled in 0 bundles, 0 large"),
        "{}",
        again.stdout
    );

    // A dry run says what it would do and does none of it.
    std::fs::remove_file(local.join("d0/f0.txt")).unwrap();
    std::fs::write(local.join("new.txt"), "new\n").unwrap();
    let dry = remote(
        &client,
        &host,
        &[
            "sync",
            "push",
            "--dry-run",
            "--delete",
            &local_text,
            &remote_text,
        ],
    );
    assert_eq!(dry.code, Some(0), "{}", dry.stderr);
    assert!(dry.stdout.contains("delete d0/f0.txt"), "{}", dry.stdout);
    assert!(dry.stdout.contains("create new.txt"), "{}", dry.stdout);
    assert!(root.join("tree/d0/f0.txt").exists());
    assert!(!root.join("tree/new.txt").exists());
    let done = remote(
        &client,
        &host,
        &["sync", "push", "--delete", &local_text, &remote_text],
    );
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert!(!root.join("tree/d0/f0.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("tree/new.txt")).unwrap(),
        "new\n"
    );

    // The listing: one line each, and JSON when asked.
    let listed = remote(&client, &host, &["walk", "--depth", "1", &remote_text]);
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let lines: Vec<&str> = listed.stdout.lines().collect();
    assert_eq!(lines.len(), 1 + 3 + 2, "{}", listed.stdout);
    assert!(lines[0].starts_with("d "), "{}", lines[0]);
    assert!(listed.stdout.contains("big.bin"), "{}", listed.stdout);
    let json = remote(
        &client,
        &host,
        &[
            "walk",
            "--format",
            "json",
            "--hash",
            "crc32c",
            "--include",
            "*.bin",
            &remote_text,
        ],
    );
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    assert!(json.stdout.contains("\"path\""), "{}", json.stdout);
    assert!(json.stdout.contains("big.bin"), "{}", json.stdout);
    let missing = remote(&client, &host, &["walk", &format!("{remote_text}/absent")]);
    assert_eq!(missing.code, Some(1), "{}", missing.stdout);

    // The other way into a directory that does not exist yet.
    let pulled_to = world.path("pulled");
    let pulled_text = pulled_to.to_string_lossy().into_owned();
    let pull = remote(
        &client,
        &host,
        &["sync", "pull", &pulled_text, &remote_text],
    );
    assert_eq!(pull.code, Some(0), "{}", pull.stderr);
    assert!(
        pull.stdout.contains("bundled in 1 bundles, 1 large"),
        "{}",
        pull.stdout
    );
    assert_eq!(
        sha256_of(&pulled_to.join("big.bin")),
        sha256_of(&local.join("big.bin"))
    );
    assert_eq!(
        std::fs::read_to_string(pulled_to.join("d1/f1.txt")).unwrap(),
        "file 1/1\n"
    );
    let quiet = remote(
        &client,
        &host,
        &["sync", "pull", &pulled_text, &remote_text],
    );
    assert!(
        quiet.stdout.contains("0 bundled in 0 bundles, 0 large"),
        "{}",
        quiet.stdout
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
    let endpoint = host.endpoint.clone();
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

    let mut hash = Hasher::new(&[Algorithm::Sha256]).unwrap();
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
        (total, hash.finish()[&Algorithm::Sha256].clone()),
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
        assert_eq!(
            (done.written, done.hashes[&Algorithm::Sha256].as_str()),
            (SIZE, want.as_str())
        );
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
    // The terminal echoes what was typed, which holds `20 + 22`; only a shell that ran it prints 42, alone or after its prompt.
    // A terminal redraws a line with title and cursor sequences before the text, so they are dropped first.
    let plain = without_escapes(&done.stdout);
    assert!(
        plain.lines().any(|line| line.split('\r').any(|part| part
            .trim()
            .trim_start_matches('$')
            .trim()
            == "42")),
        "{}",
        done.stdout
    );
}

/// `text` without its terminal escape sequences: `ESC [` ... a final byte, `ESC ]` ... BEL or `ESC \`, and `ESC` with one more byte.
fn without_escapes(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            plain.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        // A repaint moves the cursor instead of ending the line, so that is a line end here.
                        if matches!(c, 'H' | 'f' | 'B' | 'E' | 'F' | 'd') {
                            plain.push('\n');
                        }
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    plain
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
    let endpoint = host.endpoint.clone();
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

/// The settings of a client that wants `bulk` bytes of receive window on a stream that carries a file to it.
fn bulk_settings(bulk: u32) -> Settings {
    let mut settings = Settings::default();
    settings.bulk_window = bulk;
    settings.max_window = bulk;
    settings
}

/// Reads `name` whole and returns its length and SHA-256.
fn read_hash(link: &Connection, name: &str, wait: Option<Duration>) -> (u64, String) {
    let reader = Reader::start(link, &ReadRequest::whole(name), wait).unwrap();
    let mut hash = Hasher::new(&[Algorithm::Sha256]).unwrap();
    let mut total = 0_u64;
    while let Some(chunk) = reader.next_chunk(wait).unwrap() {
        hash.update(&chunk);
        total += chunk.len() as u64;
    }
    (total, hash.finish()[&Algorithm::Sha256].clone())
}

/// Writes the file `from` to `name` on the host and returns what the host says it wrote.
fn write_file(link: &Connection, from: &Path, name: &str, wait: Option<Duration>) -> (u64, String) {
    let writer = Writer::start(link, &WriteRequest::replace(name), wait).unwrap();
    let mut file = std::fs::File::open(from).unwrap();
    let mut chunk = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut chunk).unwrap();
        if count == 0 {
            break;
        }
        writer.write(&chunk[..count]).unwrap();
    }
    let done = writer.finish(wait).unwrap();
    (done.written, done.hashes[&Algorithm::Sha256].clone())
}

#[test]
fn a_read_and_a_write_with_an_8_mib_bulk_window_on_both_sides_end_without_a_broken_close() {
    const SIZE: u64 = 32 * 1024 * 1024;
    const BULK: u32 = 8 * 1024 * 1024;
    let world = World::new("bulk_window = 8388608\n");
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
    let link = connect_with(&client, &host, &bulk_settings(BULK)).expect("the link is up");
    let wait = Some(Duration::from_secs(60));
    assert_eq!(read_hash(&link, "big.bin", wait), (SIZE, want.clone()));
    assert_eq!(
        write_file(&link, &root.join("big.bin"), "copy.bin", wait),
        (SIZE, want.clone())
    );
    assert_eq!(read_hash(&link, "copy.bin", wait), (SIZE, want));
    // The host announces the window and frame it starts streams with, equal to this side's.
    let told = link.opener().capabilities(Duration::from_secs(60)).unwrap();
    assert_eq!(told.window, Some(Settings::default().window));
    assert_eq!(told.max_frame, Some(Settings::default().max_frame));
}

/// Settings that start a stream at `window` bytes of credit and grant none beyond it.
fn windowed(window: u32) -> Settings {
    let mut settings = Settings::default();
    settings.window = window;
    settings.bulk_window = window;
    settings.max_window = window;
    settings
}

#[test]
fn a_host_with_a_small_window_and_a_wide_bulk_window_serves_a_client_with_the_same_small_window() {
    const SIZE: u64 = 8 * 1024 * 1024;
    // Both sides start a stream at 64 KiB, which is the credit each assumes of the other; the host widens a bulk
    // stream it receives to 4 MiB.
    let world = World::new("window = 65536\nbulk_window = 4194304\n");
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
    let link = connect_with(&client, &host, &windowed(65536)).expect("the link is up");
    let wait = Some(Duration::from_secs(60));
    assert_eq!(
        write_file(&link, &root.join("big.bin"), "copy.bin", wait),
        (SIZE, want.clone())
    );
    assert_eq!(read_hash(&link, "copy.bin", wait), (SIZE, want));
    let told = link.opener().capabilities(Duration::from_secs(60)).unwrap();
    assert_eq!(told.window, Some(65536));
    assert_eq!(told.max_frame, Some(Settings::default().max_frame));
}

#[test]
fn a_host_with_a_smaller_window_than_the_clients_serves_it_a_read_and_announces_its_own() {
    const SIZE: u64 = 8 * 1024 * 1024;
    // The host sends with 64 KiB of credit into a client that takes 1 MiB, which is safe: only a sender whose
    // window is larger than the receiver's can overrun it, so a client with the defaults does not write to this
    // host, and the window the host announces is what lets either side see the difference.
    let world = World::new("window = 65536\nbulk_window = 4194304\n");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    let want = big_file(&root.join("big.bin"), SIZE);
    let host = world.serve("warn");
    let root_text = root.to_string_lossy().into_owned();
    let client = approved(
        &world,
        "mover",
        &["--allow", "fs.read", "--fs-root", &root_text],
    );
    let link = connect(&client, &host).expect("the link is up");
    let wait = Some(Duration::from_secs(60));
    assert_eq!(read_hash(&link, "big.bin", wait), (SIZE, want));
    let told = link.opener().capabilities(Duration::from_secs(60)).unwrap();
    assert_eq!(told.window, Some(65536));
    assert_eq!(told.max_frame, Some(Settings::default().max_frame));
}

/// How long a killed host's processes may take to be gone.
#[cfg(unix)]
const HOST_DEATH_WITHIN: Duration = Duration::from_secs(10);

/// Waits until no process runs `program` with `argument`, up to `within`; what remains is ended and returned.
#[cfg(unix)]
fn left_running(program: &str, argument: &str, within: Duration) -> Vec<u32> {
    let until = Instant::now() + within;
    loop {
        let found = support::processes_of(program, argument);
        if found.is_empty() {
            return found;
        }
        if Instant::now() >= until {
            support::end_processes(&found);
            return found;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
#[test]
fn a_host_killed_with_sigkill_leaves_no_exec_tree() {
    let world = World::new("");
    let mut host = world.serve("warn");
    let client = approved(&world, "ops", &["--allow", "info,exec"]);
    // A marker no other test's helper has, so the scan sees only this test's tree.
    let ms = (600_000 + u64::from(std::process::id() % 5000)).to_string();
    let helper = helper();
    let mut running = Proc::start(
        &[
            "remote",
            &host.endpoint,
            "--config",
            &client.config,
            "exec",
            "--",
            &helper,
            "tree",
            &ms,
        ],
        None,
        "warn",
    );
    // The helper prints its child's id once both run.
    let grandchild: u32 = running.expect_line("").parse().expect("a process id");
    assert_eq!(
        support::processes_of(&helper, &ms).len(),
        2,
        "the control: the scan sees the program and its child while the host lives"
    );
    assert!(alive(grandchild));
    host.process.kill();
    let left = left_running(&helper, &ms, HOST_DEATH_WITHIN);
    assert!(
        left.is_empty(),
        "processes of the exec tree outlived the killed host: {left:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_host_killed_with_sigkill_leaves_no_shell_tree() {
    // On Linux a shell's whole session is ended, so the job a shell with job control puts in a group of
    // its own is too. Elsewhere only the shell's group is: the job stays in it with job control off.
    let monitor = if cfg!(any(target_os = "linux", target_os = "android")) {
        ""
    } else {
        "set +m\r"
    };
    let world = World::new("");
    let mut host = world.serve("warn");
    let client = approved(&world, "ops", &["--allow", "shell"]);
    let ms = (700_000 + u64::from(std::process::id() % 5000)).to_string();
    let shell = Proc::start_with(
        &[
            "remote",
            &host.endpoint,
            "--config",
            &client.config,
            "shell",
            "--kind",
            "sh",
        ],
        Some(&format!("{monitor}sleep {ms} &\recho $((40 + 2))\r")),
        true,
        "warn",
    );
    let until = Instant::now() + DEADLINE;
    while !text(&shell.stdout).lines().any(|line| {
        line.split('\r')
            .any(|part| !part.contains("echo") && part.split_whitespace().last() == Some("42"))
    }) {
        assert!(
            Instant::now() < until,
            "the shell did not answer\n{}",
            text(&shell.stdout)
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        support::processes_of("sleep", &ms).len(),
        1,
        "the control: the scan sees the shell's background job while the host lives"
    );
    host.process.kill();
    let left = left_running("sleep", &ms, HOST_DEATH_WITHIN);
    assert!(
        left.is_empty(),
        "processes of the shell tree outlived the killed host: {left:?}"
    );
    drop(shell);
}

/// The ids of the processes of this test's host: it and the guardians it made, which carry its command line.
#[cfg(unix)]
fn host_processes(config: &str) -> Vec<u32> {
    support::processes_of(env!("CARGO_BIN_EXE_bohio"), config)
}

#[cfg(unix)]
#[test]
fn a_program_that_ended_leaves_no_guardian_behind() {
    let world = World::new("");
    let host = world.serve("warn");
    let client = approved(&world, "ops", &["--allow", "info,exec"]);
    let ms = (610_000 + u64::from(std::process::id() % 5000)).to_string();
    let helper = helper();
    let mut running = Proc::start(
        &[
            "remote",
            &host.endpoint,
            "--config",
            &client.config,
            "exec",
            "--",
            &helper,
            "tree",
            &ms,
        ],
        None,
        "warn",
    );
    running.expect_line("");
    assert_eq!(
        host_processes(&world.config).len(),
        2,
        "the control: while a program runs the host has its guardian beside it"
    );
    // Ending the request ends the tree, and the guardian is in it.
    drop(running);
    let until = Instant::now() + DEADLINE;
    while host_processes(&world.config).len() > 1 && Instant::now() < until {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        host_processes(&world.config),
        vec![host.process.id()],
        "a guardian outlived its program"
    );
}

#[cfg(unix)]
#[test]
fn a_host_set_to_leave_its_programs_leaves_them_when_killed() {
    let world = World::new("\n[execution]\nhost_death = \"leave\"\n");
    let mut host = world.serve("warn");
    let client = approved(&world, "ops", &["--allow", "info,exec"]);
    let ms = (620_000 + u64::from(std::process::id() % 5000)).to_string();
    let helper = helper();
    let mut running = Proc::start(
        &[
            "remote",
            &host.endpoint,
            "--config",
            &client.config,
            "exec",
            "--",
            &helper,
            "tree",
            &ms,
        ],
        None,
        "warn",
    );
    running.expect_line("");
    assert_eq!(
        host_processes(&world.config),
        vec![host.process.id()],
        "no guardian is made under `leave`"
    );
    host.process.kill();
    thread::sleep(Duration::from_secs(2));
    let left = support::processes_of(&helper, &ms);
    support::end_processes(&left);
    assert_eq!(left.len(), 2, "`leave` left the tree running: {left:?}");
}

#[cfg(unix)]
#[test]
fn a_host_set_to_term_ends_its_programs_with_sigterm_when_killed() {
    let world = World::new("\n[execution]\nhost_death = \"term\"\n");
    let mut host = world.serve("warn");
    let client = approved(&world, "ops", &["--allow", "info,exec"]);
    let ms = (630_000 + u64::from(std::process::id() % 5000)).to_string();
    let helper = helper();
    let mut running = Proc::start(
        &[
            "remote",
            &host.endpoint,
            "--config",
            &client.config,
            "exec",
            "--",
            &helper,
            "tree",
            &ms,
        ],
        None,
        "warn",
    );
    running.expect_line("");
    host.process.kill();
    let left = left_running(&helper, &ms, HOST_DEATH_WITHIN);
    assert!(left.is_empty(), "SIGTERM left the tree running: {left:?}");
}

#[test]
fn the_command_searches_globs_and_reports_the_host_with_no_exec_granted() {
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    for dir in 0..10 {
        for file in 0..100 {
            let path = root.join(format!("d{dir}/f{file}.txt"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let marker = if (dir, file) == (7, 42) {
                "needle"
            } else {
                "hay"
            };
            std::fs::write(path, format!("first\nline {dir}/{file} {marker}\nlast\n")).unwrap();
        }
    }
    std::fs::write(root.join("d0/f0.bin"), b"needle\0binary").unwrap();
    let host = world.serve("warn");
    let root_text = root.to_string_lossy().into_owned();
    let client = approved(
        &world,
        "searcher",
        &["--allow", "info,fs.read", "--fs-root", &root_text],
    );

    // 1000 files, one match, on a peer that holds neither exec nor fs.write.
    let found = remote(&client, &host, &["search", "needle", &root_text]);
    assert_eq!(found.code, Some(0), "{}", found.stderr);
    let expected = format!("{root_text}/d7/f42.txt:2:line 7/42 needle");
    assert_eq!(found.stdout.trim_end(), expected, "{}", found.stdout);
    let around = remote(&client, &host, &["search", "-C", "1", "needle", &root_text]);
    assert_eq!(around.code, Some(0), "{}", around.stderr);
    let lines: Vec<&str> = around.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{}", around.stdout);
    assert!(lines[0].ends_with("-1-first"), "{}", lines[0]);
    assert!(lines[2].ends_with("-3-last"), "{}", lines[2]);
    let folded = remote(
        &client,
        &host,
        &["search", "-i", "-F", "NEEDLE", &root_text, "-g", "*.txt"],
    );
    assert_eq!(folded.code, Some(0), "{}", folded.stderr);
    assert_eq!(folded.stdout.lines().count(), 1);
    // No match is success, the zero visible in the counts, and a request the host refuses is an error.
    let none = remote(&client, &host, &["search", "absent", &root_text]);
    assert_eq!(none.code, Some(0), "{}", none.stderr);
    assert!(none.stdout.is_empty(), "{}", none.stdout);
    let none_json = remote(
        &client,
        &host,
        &["search", "--format", "json", "absent", &root_text],
    );
    assert_eq!(none_json.code, Some(0), "{}", none_json.stderr);
    let last: serde_json::Value =
        serde_json::from_str(none_json.stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["event"], "done");
    assert_eq!(last["matches"], 0, "{}", none_json.stdout);
    let bad = remote(&client, &host, &["search", "(", &root_text]);
    assert_eq!(bad.code, Some(1), "{}", bad.stdout);
    assert!(bad.stderr.contains("refused"), "{}", bad.stderr);
    let json = remote(
        &client,
        &host,
        &["search", "--format", "json", "needle", &root_text],
    );
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    // One event for the match, and the host's counts last.
    let lines: Vec<serde_json::Value> = json
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{}", json.stdout);
    assert_eq!(lines[0]["event"], "match");
    assert_eq!(lines[0]["line"], 2);
    assert_eq!(lines[0]["text"], "line 7/42 needle");
    assert_eq!(lines[1]["event"], "done");

    // The same tree by name.
    let listed = remote(&client, &host, &["glob", "d3/f1*.txt", &root_text]);
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert_eq!(listed.stdout.lines().count(), 11, "{}", listed.stdout);
    assert!(
        listed
            .stdout
            .lines()
            .all(|line| line.starts_with(&format!("{root_text}/d3/f1")))
    );

    // What the host runs on, and where its programs are.
    let stats = remote(&client, &host, &["stats", "--format", "json"]);
    assert_eq!(stats.code, Some(0), "{}", stats.stderr);
    let stats: serde_json::Value = serde_json::from_str(stats.stdout.trim()).unwrap();
    assert!(stats["memory"]["total"].as_u64().unwrap() > 0, "{stats}");
    assert!(!stats["disks"].as_array().unwrap().is_empty(), "{stats}");
    let text = remote(&client, &host, &["stats"]);
    assert_eq!(text.code, Some(0), "{}", text.stderr);
    assert!(text.stdout.contains("memory"), "{}", text.stdout);
    let program = if cfg!(windows) { "cmd" } else { "sh" };
    let which = remote(&client, &host, &["which", program, "no-such-program-here"]);
    assert_eq!(which.code, Some(1), "{}", which.stderr);
    let lines: Vec<&str> = which.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{}", which.stdout);
    assert!(
        lines[0].starts_with(program) && !lines[0].ends_with("  -"),
        "{}",
        lines[0]
    );
    assert_eq!(lines[1], "no-such-program-here  -");

    // The host refused these peers nothing it should not: a peer without `info` gets neither.
    let reader = approved(
        &world,
        "readonly",
        &["--allow", "fs.read", "--fs-root", &root_text],
    );
    let denied = remote(&reader, &host, &["stats"]);
    assert_eq!(denied.code, Some(1), "{}", denied.stdout);
    assert!(denied.stderr.contains("info"), "{}", denied.stderr);
}

/// A file the host cannot read fails the search (exit 1) while the matches it did find are still printed; a search
/// that matches nothing is exit 0.
#[cfg(unix)]
#[test]
fn a_search_that_could_not_read_a_file_exits_1_and_one_that_found_nothing_exits_0() {
    use std::os::unix::fs::PermissionsExt;
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("open.txt"),
        "a needle
",
    )
    .unwrap();
    let locked = root.join("locked.txt");
    std::fs::write(
        &locked,
        "a needle
",
    )
    .unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // A superuser reads it anyway, and then there is nothing to fail.
    let unreadable = std::fs::read(&locked).is_err();
    let host = world.serve("warn");
    let root_text = root.to_string_lossy().into_owned();
    let client = approved(
        &world,
        "searcher",
        &["--allow", "info,fs.read", "--fs-root", &root_text],
    );
    let found = remote(&client, &host, &["search", "needle", &root_text]);
    let nothing = remote(&client, &host, &["search", "absent", &root_text]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        found.stdout.contains("open.txt:1:a needle"),
        "{}",
        found.stdout
    );
    if unreadable {
        assert_eq!(found.code, Some(1), "{}", found.stderr);
        assert!(found.stderr.contains("locked.txt"), "{}", found.stderr);
        assert_eq!(nothing.code, Some(1), "{}", nothing.stderr);
    } else {
        assert_eq!(found.code, Some(0), "{}", found.stderr);
        assert_eq!(nothing.code, Some(0), "{}", nothing.stderr);
    }
    let clean = remote(
        &client,
        &host,
        &["search", "absent", &format!("{root_text}/open.txt")],
    );
    assert_eq!(clean.code, Some(0), "{}", clean.stderr);
    let wrong = remote(&client, &host, &["search"]);
    assert_eq!(wrong.code, Some(2), "{}", wrong.stderr);
}

#[test]
fn a_revoke_ends_a_running_follow_within_recheck() {
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("app.log"), "start\n").unwrap();
    let host = world.serve("warn");
    let root_text = root.to_string_lossy().into_owned();
    let client = approved(
        &world,
        "tailer",
        &["--allow", "fs.read", "--fs-root", &root_text],
    );
    let link = connect(&client, &host).expect("the link is up");
    let wait = Some(Duration::from_secs(20));
    let options = bohio_client::follow::FollowOptions {
        interval: Some(Duration::from_millis(50)),
        ..bohio_client::follow::FollowOptions::default()
    };
    let mut following = bohio_client::follow::follow(&link, "app.log", &options, wait).unwrap();
    let first = following.next_within(wait).unwrap();
    assert_eq!(
        first,
        Some(bohio_client::follow::FollowEvent::Data(b"start\n".to_vec()))
    );
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("app.log"))
        .unwrap();
    log.write_all(b"more\n").unwrap();
    log.sync_all().unwrap();
    assert_eq!(
        following.next_within(wait).unwrap(),
        Some(bohio_client::follow::FollowEvent::Data(b"more\n".to_vec()))
    );

    world.revoke(&client);
    let (took, error) = ends_with_error(
        || following.next_within(Some(Duration::from_millis(100))),
        "the follow",
    );
    eprintln!("follow ended {took:?} after the revoke (recheck {RECHECK:?})");
    assert!(is_refusal(&error), "{error:?}");
    assert!(took <= RECHECK + SLACK, "took {took:?}");
}

/// `docs/cli-json.md`'s table of shapes: each name with the keys its output always has.
fn json_shapes() -> std::collections::BTreeMap<String, (String, Vec<String>)> {
    let page = include_str!("../../../docs/cli-json.md");
    let block = page
        .split("```shapes\n")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("the page has a shapes block");
    block
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split(" | ").collect();
            assert_eq!(parts.len(), 3, "a shape is `NAME | KIND | KEYS`: {line}");
            (
                parts[0].trim().to_owned(),
                (
                    parts[1].trim().to_owned(),
                    parts[2]
                        .split(',')
                        .map(|key| key.trim().to_owned())
                        .collect(),
                ),
            )
        })
        .collect()
}

/// The lines of `output` as JSON objects.
fn json_lines(output: &str) -> Vec<serde_json::Value> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("`{line}` is not JSON: {error}"))
        })
        .collect()
}

/// Checks `value` has every key the page lists for `shape`, and that the page has the shape.
fn has_shape(
    shapes: &std::collections::BTreeMap<String, (String, Vec<String>)>,
    shape: &str,
    value: &serde_json::Value,
) {
    let (_, keys) = shapes
        .get(shape)
        .unwrap_or_else(|| panic!("docs/cli-json.md has no shape `{shape}`"));
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("`{shape}` is not an object: {value}"));
    for key in keys {
        assert!(object.contains_key(key), "`{shape}` lacks `{key}`: {value}");
    }
}

/// The event of that name among `lines`, checked against the shape `shape/event`.
fn has_event(
    shapes: &std::collections::BTreeMap<String, (String, Vec<String>)>,
    command: &str,
    event: &str,
    lines: &[serde_json::Value],
) {
    let found = lines
        .iter()
        .find(|line| line["event"] == event)
        .unwrap_or_else(|| panic!("no `{event}` event of `{command}` in {lines:?}"));
    has_shape(shapes, &format!("{command}/{event}"), found);
}

/// Every command, run with `--format json`, prints what `docs/cli-json.md` says (and nothing else on standard output).
#[test]
fn every_command_prints_json_that_docs_cli_json_describes() {
    let shapes = json_shapes();
    let world = World::new("\n[remote.demo]\nendpoint = \"tcp://127.0.0.1:1\"\n");
    let root = world.path("files");
    std::fs::create_dir_all(root.join("tree")).unwrap();
    std::fs::write(root.join("tree/a.txt"), "needle one\nsecond\n").unwrap();
    std::fs::write(root.join("note.txt"), "hello\n").unwrap();
    let host = world.serve("warn");
    let root_text = root.to_string_lossy().into_owned();
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let target_port = target.local_addr().unwrap().port().to_string();
    let client = approved(
        &world,
        "jsonman",
        &[
            "--allow",
            "info,exec,shell,fs.read,fs.write,session",
            "--fs-root",
            &root_text,
            "--connect",
            "127.0.0.1:*",
        ],
    );
    let config = world.config.clone();

    // A command's one object, checked against the page.
    let object_status = |finished: Finished, shape: &str, status: i32| {
        assert_eq!(finished.code, Some(status), "{shape}: {}", finished.stderr);
        let value: serde_json::Value =
            serde_json::from_str(finished.stdout.trim()).unwrap_or_else(|error| {
                panic!("{shape}: `{}` is one object: {error}", finished.stdout)
            });
        has_shape(&shapes, shape, &value);
        value
    };
    let object = |finished: Finished, shape: &str| object_status(finished, shape, 0);

    // The node and its files.
    object(
        run(&["config", "check", "--config", &config, "--format", "json"]),
        "config check",
    );
    object(
        run(&["info", "--config", &config, "--format", "json"]),
        "info",
    );
    let id = object(run(&["id", "--config", &config, "--format", "json"]), "id");
    assert_eq!(id["id"], host.id);
    object(
        run(&["status", "--config", &config, "--format", "json"]),
        "status",
    );
    let listed = object(
        run(&["peer", "list", "--config", &config, "--format", "json"]),
        "peer list",
    );
    assert!(
        listed["accepted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|peer| peer["id"] == client.id
                && peer["allow"].as_array().is_some_and(|a| !a.is_empty())),
        "{listed}"
    );
    let hosts = object(
        run(&["hosts", "--config", &config, "--format", "json"]),
        "hosts",
    );
    assert_eq!(hosts["hosts"][0]["name"], "demo");
    assert_eq!(hosts["hosts"][0]["endpoint"], "tcp://127.0.0.1:1");
    object(
        run(&["links", "--config", &config, "--format", "json"]),
        "links",
    );
    object(
        run(&["providers", "--config", &config, "--format", "json"]),
        "providers",
    );

    // A peer that pairs, is approved and is revoked, as objects; the pairing as events.
    let joiner = world.client("joiner");
    let mut door = Proc::start(
        &[
            "pair",
            "--config",
            &config,
            "--timeout",
            "60s",
            "--format",
            "json",
        ],
        None,
        "warn",
    );
    let pin = door.expect_line("{\"event\":\"host\"");
    assert!(!pin.is_empty());
    let until = Instant::now() + DEADLINE;
    let pin = loop {
        let lines = json_lines(&text(&door.stdout));
        if let Some(line) = lines.iter().find(|line| line["event"] == "pin") {
            break line["pin"].as_str().unwrap().to_owned();
        }
        assert!(Instant::now() < until, "no pin event");
        thread::sleep(Duration::from_millis(20));
    };
    let joined = run(&[
        "join",
        &host.endpoint,
        "--pin",
        &pin,
        "--config",
        &joiner.config,
        "--format",
        "json",
    ]);
    let joined = object(joined, "join");
    assert_eq!(joined["paired"], host.id);
    let done = door.finish();
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    let lines = json_lines(&done.stdout);
    has_event(&shapes, "pair", "host", &lines);
    has_event(&shapes, "pair", "pin", &lines);
    assert!(
        lines
            .iter()
            .any(|line| line["event"] == "paired" && line["peer"] == joiner.id)
    );
    let approved_json = object(
        run(&[
            "peer", "approve", &joiner.id, "--allow", "info", "--config", &config, "--format",
            "json",
        ]),
        "peer approve",
    );
    assert_eq!(approved_json["approved"]["id"], joiner.id);
    object(
        run(&[
            "peer", "revoke", &joiner.id, "--config", &config, "--format", "json",
        ]),
        "peer revoke",
    );

    // `serve` and `pair` on a node of their own stream events.
    let other = World::new("");
    let mut serving = Proc::start(
        &[
            "serve",
            "--config",
            &other.config,
            "--listen",
            "tcp://127.0.0.1:0",
            "--format",
            "json",
        ],
        None,
        "warn",
    );
    serving.expect_line("{\"endpoint\":");
    let until = Instant::now() + DEADLINE;
    let lines = loop {
        let lines = json_lines(&text(&serving.stdout));
        if lines.iter().any(|line| line["event"] == "host") {
            break lines;
        }
        assert!(Instant::now() < until, "no host event");
        thread::sleep(Duration::from_millis(20));
    };
    has_event(&shapes, "serve", "listening", &lines);
    has_event(&shapes, "serve", "host", &lines);
    drop(serving);
    let alone = world.path("alone.cfg");
    std::fs::write(
        &alone,
        format!(
            "[service]\ndata_dir = {}\n[control]\nenabled = false\n[[listen]]\nendpoint = \"tcp://127.0.0.1:0\"\n",
            toml_path(&world.path("alone-data"))
        ),
    )
    .unwrap();
    let pairing = Proc::start(
        &[
            "pair",
            "--config",
            &alone.to_string_lossy(),
            "--timeout",
            "60s",
            "--format",
            "json",
        ],
        None,
        "warn",
    );
    let until = Instant::now() + DEADLINE;
    let lines = loop {
        let lines = json_lines(&text(&pairing.stdout));
        if lines.iter().any(|line| line["event"] == "pin") {
            break lines;
        }
        assert!(Instant::now() < until, "no pin event");
        thread::sleep(Duration::from_millis(20));
    };
    has_event(&shapes, "pair", "pairing", &lines);
    drop(pairing);

    // What a peer asks of the host.
    let ask = |args: &[&str]| {
        let mut all = vec!["--format", "json"];
        all.extend_from_slice(args);
        remote(&client, &host, &all)
    };
    object(ask(&["info"]), "remote info");
    object(ask(&["stats"]), "remote stats");
    let program = if cfg!(windows) { "cmd" } else { "sh" };
    let found = object_status(
        ask(&["which", program, "no-such-program-here"]),
        "remote which",
        1,
    );
    assert!(found["found"]["no-such-program-here"].is_null());
    let note = format!("{root_text}/note.txt");
    let written = object(
        ask(&["write", &format!("{root_text}/written.txt")]),
        "remote write",
    );
    assert_eq!(written["written"], 0);
    let local = world.path("local.txt");
    std::fs::write(&local, "from here\n").unwrap();
    let local_text = local.to_string_lossy().into_owned();
    object(
        ask(&["put", &local_text, &format!("{root_text}/put.txt")]),
        "remote put",
    );
    let back = world.path("back.txt");
    object(
        ask(&[
            "get",
            &format!("{root_text}/put.txt"),
            &back.to_string_lossy(),
        ]),
        "remote get",
    );
    assert_eq!(std::fs::read_to_string(&back).unwrap(), "from here\n");
    let tree = world.path("tree-local");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("one.txt"), "1\n").unwrap();
    object(
        ask(&[
            "sync",
            "push",
            &tree.to_string_lossy(),
            &format!("{root_text}/synced"),
        ]),
        "remote sync",
    );

    // Streams.
    let helper = helper();
    let exec = ask(&["exec", "--", &helper, "echo", "hi"]);
    assert_eq!(exec.code, Some(0), "{}", exec.stderr);
    let lines = json_lines(&exec.stdout);
    has_event(&shapes, "remote exec", "started", &lines);
    has_event(&shapes, "remote exec", "stdout", &lines);
    has_event(&shapes, "remote exec", "exit", &lines);
    assert_eq!(lines.last().unwrap()["event"], "exit");
    let joined: String = lines
        .iter()
        .filter(|line| line["event"] == "stdout")
        .map(|line| line["b64"].as_str().unwrap())
        .collect();
    assert_eq!(joined, "aGkK", "`hi` and a line end in base64");
    let both = ask(&["exec", "--", &helper, "both"]);
    has_event(&shapes, "remote exec", "stderr", &json_lines(&both.stdout));
    let hashed = ask(&["hash", &note, &format!("{root_text}/missing.txt")]);
    assert_eq!(hashed.code, Some(1), "{}", hashed.stdout);
    let lines = json_lines(&hashed.stdout);
    has_event(&shapes, "remote hash", "hash", &lines);
    has_event(&shapes, "remote hash", "failure", &lines);
    let walked = ask(&["walk", &root_text]);
    assert_eq!(walked.code, Some(0), "{}", walked.stderr);
    has_event(&shapes, "remote walk", "entry", &json_lines(&walked.stdout));
    let globbed = ask(&["glob", "**/*.txt", &root_text]);
    assert_eq!(globbed.code, Some(0), "{}", globbed.stderr);
    has_event(&shapes, "remote glob", "path", &json_lines(&globbed.stdout));
    let searched = ask(&["search", "needle", &root_text]);
    assert_eq!(searched.code, Some(0), "{}", searched.stderr);
    let lines = json_lines(&searched.stdout);
    has_event(&shapes, "remote search", "match", &lines);
    has_event(&shapes, "remote search", "done", &lines);

    // Sessions.
    let detached = object(
        ask(&["exec", "--detach", "--", &helper, "sleep", "60000"]),
        "remote exec --detach",
    );
    let session = detached["session"].as_str().unwrap().to_owned();
    let sessions = object(ask(&["sessions"]), "remote sessions");
    assert!(
        sessions["sessions"]
            .as_array()
            .is_some_and(|all| all.iter().any(|one| one["id"] == session))
    );
    let ended = object(ask(&["end", &session]), "remote end");
    assert_eq!(ended["ended"], session);

    // A connection, a forward and a read whose payload is the file.
    object(
        ask(&["connect", "--check", &format!("127.0.0.1:{target_port}")]),
        "remote connect --check",
    );
    let mut forwarding = Proc::start(
        &[
            "remote",
            &host.endpoint,
            "--config",
            &client.config,
            "forward",
            "-L",
            &format!("127.0.0.1:0=127.0.0.1:{target_port}"),
            "--format",
            "json",
        ],
        None,
        "warn",
    );
    forwarding.expect_line("{\"address\":");
    let lines = json_lines(&text(&forwarding.stdout));
    has_event(&shapes, "remote forward", "listening", &lines);
    drop(forwarding);
    let read = ask(&["read", &note]);
    assert_eq!(read.code, Some(0), "{}", read.stderr);
    assert_eq!(read.stdout, "hello\n", "the payload is unchanged");
    has_shape(&shapes, "remote read/done", &json_lines(&read.stderr)[0]);
    let pty = ask(&["exec", "--pty", "--", &helper, "echo", "tty"]);
    assert_eq!(pty.code, Some(0), "{}", pty.stderr);
    let done = json_lines(&pty.stderr);
    has_shape(&shapes, "remote shell/done", done.last().unwrap());
    assert!(done.last().unwrap()["done"]["exit"].is_object());
    object(ask(&["gui", "info"]), "remote gui info");

    // An error is one object on standard error and nothing on standard output.
    let missing = ask(&["read", &format!("{root_text}/absent.txt")]);
    assert_eq!(missing.code, Some(1));
    assert!(missing.stdout.is_empty(), "{}", missing.stdout);
    let error = &json_lines(&missing.stderr)[0];
    has_shape(&shapes, "error", error);
    assert_eq!(error["error"]["code"], "not_found");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
    let bad = run(&[
        "config",
        "check",
        "--config",
        "definitely-not-here.cfg",
        "--format",
        "json",
    ]);
    assert_eq!(bad.code, Some(1));
    assert_eq!(json_lines(&bad.stderr)[0]["error"]["code"], "failed");
    drop(target);
}

#[test]
fn a_client_profile_sets_what_the_flags_leave_out() {
    let world = World::new("");
    let root = world.path("files");
    std::fs::create_dir_all(&root).unwrap();
    let host = world.serve("warn");
    let client = approved(
        &world,
        "profiled",
        &[
            "--allow",
            "fs.read,fs.write",
            "--fs-root",
            &root.to_string_lossy(),
        ],
    );
    let text: String = (0..60_000)
        .map(|n| format!("line {n}: the quick brown fox {} jumps\n", n % 17))
        .collect();
    let local = world.path("local.txt");
    std::fs::write(&local, &text).unwrap();
    let size = text.len() as u64;
    let local_text = local.to_string_lossy().into_owned();
    let theirs = |name: &str| root.join(name).to_string_lossy().into_owned();
    let wire_of = |done: &Finished| {
        assert_eq!(done.code, Some(0), "{}", done.stderr);
        let (wire, total, _, _) = sent_line(&done.stdout);
        assert_eq!(total, size, "{}", done.stdout);
        wire
    };

    // Nothing asks for compression: the file travels whole.
    let plain = remote(&client, &host, &["put", &local_text, &theirs("a.txt")]);
    assert!(wire_of(&plain) >= size, "{}", plain.stdout);

    // A weak machine's profile asks for lz4; the file shrinks.
    let weak = remote(
        &client,
        &host,
        &[
            "--profile",
            "weak-host",
            "put",
            &local_text,
            &theirs("b.txt"),
        ],
    );
    assert!(wire_of(&weak) * 2 < size, "{}", weak.stdout);
    assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), text);

    // A flag beats the profile.
    let flagged = remote(
        &client,
        &host,
        &[
            "--profile",
            "weak-host",
            "put",
            "--compress",
            "none",
            &local_text,
            &theirs("c.txt"),
        ],
    );
    assert!(wire_of(&flagged) >= size, "{}", flagged.stdout);

    // The profile in the client's own file applies without a flag, and `--profile default` puts it back.
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(&client.config)
        .unwrap();
    config
        .write_all(b"\n[client]\nprofile = \"weak-host\"\n")
        .unwrap();
    drop(config);
    let from_file = remote(&client, &host, &["put", &local_text, &theirs("d.txt")]);
    assert!(wire_of(&from_file) * 2 < size, "{}", from_file.stdout);
    let back = remote(
        &client,
        &host,
        &["--profile", "default", "put", &local_text, &theirs("e.txt")],
    );
    assert!(wire_of(&back) >= size, "{}", back.stdout);

    // The same for a get.
    let pulled = world.path("pulled.txt");
    let got = remote(
        &client,
        &host,
        &["get", &theirs("a.txt"), &pulled.to_string_lossy()],
    );
    assert!(wire_of(&got) * 2 < size, "{}", got.stdout);
    assert_eq!(std::fs::read_to_string(&pulled).unwrap(), text);

    // A name that is no profile is refused before anything connects.
    let unknown = remote(&client, &host, &["--profile", "turbo", "info"]);
    assert_eq!(unknown.code, Some(2), "{}", unknown.stderr);
    assert!(
        unknown.stderr.contains("not a profile") && unknown.stderr.contains("slow-link"),
        "{}",
        unknown.stderr
    );
}
