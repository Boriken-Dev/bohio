//! A link over every transport: `bohio serve` runs as a child process listening on a TCP socket, a
//! named pipe or Unix socket, a WebSocket, a serial line or its own standard input and output, and a
//! `bohio remote` child proves itself, runs `info` and a small `exec`. The proof, the admission and the
//! engines are the same for each; an unapproved key is refused over each non-TCP transport too.
//!
//! Every wait has a deadline, every child is killed when its handle drops, and every data directory is
//! under the system temporary directory.

mod support;

use std::io::Read;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use support::helper;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tempfile::TempDir;

/// A wait long enough for any exchange on this machine, short enough to fail a hung test.
const DEADLINE: Duration = Duration::from_secs(60);

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
    /// Starts `program` with `args`; `piped_stdin` keeps its input open (it is closed otherwise).
    fn start(program: &str, args: &[&str], piped_stdin: bool, log: &str) -> Self {
        let mut command = Command::new(program);
        command
            .args(args)
            .env_remove("BOHIO_CONFIG")
            .env("BOHIO_LOG", log)
            .env("NO_COLOR", "1")
            .stdin(if piped_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("the program starts");
        let (stdout, out_reader) = collect(child.stdout.take().unwrap());
        let (stderr, err_reader) = collect(child.stderr.take().unwrap());
        Self {
            child,
            stdout,
            stderr,
            readers: vec![out_reader, err_reader],
        }
    }

    fn bohio(args: &[&str]) -> Self {
        Self::start(env!("CARGO_BIN_EXE_bohio"), args, false, "warn")
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
                panic!(
                    "the child ended ({status}) before printing `{prefix}`\nstdout: {}\nstderr: {}",
                    text(&self.stdout),
                    text(&self.stderr)
                );
            }
            assert!(
                Instant::now() < until,
                "no `{prefix}` line within the deadline\nstdout: {}\nstderr: {}",
                text(&self.stdout),
                text(&self.stderr)
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
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(args: &[&str]) -> Finished {
    Proc::bohio(args).finish()
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

fn toml_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// A host and a client, each with its own configuration and data directory, the host's `link` section
/// `extra` and both proving within five seconds.
struct World {
    dir: TempDir,
    host: String,
    client: String,
    host_id: String,
    client_id: String,
}

impl World {
    fn new(host_extra: &str) -> Self {
        let dir = tempfile::tempdir().expect("a directory");
        let mut configs = Vec::new();
        for (name, extra) in [("host", host_extra), ("client", "")] {
            let data = dir.path().join(format!("{name}-data"));
            let config = dir.path().join(format!("{name}.cfg"));
            std::fs::write(
                &config,
                format!(
                    "[service]\nname = \"{name}\"\ndata_dir = {}\n{extra}\n[link]\nrecheck = \"1s\"\nproof_timeout = \"5s\"\n",
                    toml_path(&data)
                ),
            )
            .expect("the configuration is written");
            configs.push(config.to_string_lossy().into_owned());
        }
        let client = configs.pop().unwrap();
        let host = configs.pop().unwrap();
        let id = |config: &str| run_ok(&["id", "--config", config]).stdout.trim().to_owned();
        Self {
            host_id: id(&host),
            client_id: id(&client),
            host,
            client,
            dir,
        }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }

    /// Starts `bohio serve` listening on `endpoint` and waits for it to say where.
    fn serve(&self, endpoint: &str) -> (Proc, String) {
        let mut host = Proc::bohio(&["serve", "--config", &self.host, "--listen", endpoint]);
        let bound = host.expect_line("listening ");
        (host, bound)
    }

    /// `bohio remote` for the client against `target` (an endpoint, or `--command` and its text),
    /// trusting the host by its id.
    fn remote(&self, target: &[&str], action: &[&str]) -> Finished {
        let mut args = vec![
            "remote",
            "--config",
            &self.client,
            "--server",
            &self.host_id,
        ];
        args.extend_from_slice(target);
        args.extend_from_slice(action);
        run(&args)
    }

    /// As [`World::remote`], tried up to `attempts` times until it succeeds: a serial line carries
    /// what a reopening host has not yet taken.
    fn remote_ok(&self, target: &[&str], action: &[&str], attempts: usize) -> Finished {
        let mut last = None;
        for _ in 0..attempts {
            let done = self.remote(target, action);
            if done.code == Some(0) {
                return done;
            }
            last = Some(done);
        }
        let done = last.expect("at least one attempt");
        panic!(
            "bohio remote {target:?} {action:?} failed ({:?})\nstdout: {}\nstderr: {}",
            done.code, done.stdout, done.stderr
        );
    }

    /// As [`World::remote`], again while the status is not `code` and attempts are left: a serial line
    /// drops what a reopening host has not yet taken, and a refused link ends with status 1.
    fn remote_until(
        &self,
        target: &[&str],
        action: &[&str],
        attempts: usize,
        code: i32,
    ) -> Finished {
        let mut done = self.remote(target, action);
        for _ in 1..attempts {
            if done.code == Some(code) {
                break;
            }
            done = self.remote(target, action);
        }
        done
    }

    fn peers(&self) -> String {
        run_ok(&["peer", "list", "--config", &self.host]).stdout
    }

    /// The whole scenario over `target`: an unapproved key is refused and waits; once approved the
    /// peer gets `info`, runs a program and sees its status.
    fn exercise(&self, target: &[&str], attempts: usize) {
        // The host has never heard of this key: the proof succeeds and the host refuses it.
        let refused = self.remote(target, &["info"]);
        assert_eq!(
            refused.code,
            Some(1),
            "an unapproved key got in\nstdout: {}\nstderr: {}",
            refused.stdout,
            refused.stderr
        );
        assert!(
            refused.stdout.is_empty(),
            "a refused peer was told something: {}",
            refused.stdout
        );
        assert!(
            self.peers()
                .contains(&format!("waiting {}", self.client_id)),
            "the refused key is not listed as waiting: {}",
            self.peers()
        );

        // A file several link frames long (`max_frame` is 64 KiB): every frame, with its seal, must fit the carrier.
        let files = self.path("files");
        std::fs::create_dir_all(&files).unwrap();
        let big: String = (0..60_000)
            .map(|n| {
                format!(
                    "line {n:06}
"
                )
            })
            .collect();
        assert!(big.len() > 8 * 64 * 1024);
        std::fs::write(files.join("big.txt"), &big).unwrap();
        let root = files.to_string_lossy().into_owned();
        run_ok(&[
            "peer",
            "approve",
            &self.client_id,
            "--config",
            &self.host,
            "--allow",
            "info,exec,fs.read",
            "--fs-root",
            &root,
        ]);

        let info = self.remote_ok(target, &["info", "--format", "json"], attempts);
        assert!(
            info.stdout
                .contains(&format!("\"host_id\":\"{}\"", self.host_id)),
            "{}",
            info.stdout
        );
        let helper = helper();
        let echo = self.remote_ok(
            target,
            &["exec", "--", &helper, "echo", "hello", "world"],
            attempts,
        );
        assert_eq!(echo.stdout.trim(), "hello world", "{}", echo.stderr);
        let path = files.join("big.txt").to_string_lossy().into_owned();
        let read = self.remote_ok(target, &["read", &path], attempts);
        assert!(
            read.stdout == big,
            "the file read over the link differs: {} bytes of {}",
            read.stdout.len(),
            big.len()
        );
        let failing = self.remote_until(target, &["exec", "--", &helper, "exit", "3"], attempts, 3);
        assert_eq!(
            failing.code,
            Some(3),
            "the program's status is the command's\nstderr: {}",
            failing.stderr
        );
    }
}

/// The processes whose command line holds every text of `needles`.
fn processes_with(needles: &[&str]) -> Vec<u32> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    system
        .processes()
        .values()
        .filter(|process| {
            // Linux lists a process's threads beside it, with the same command line.
            if process.thread_kind().is_some() {
                return false;
            }
            let line = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            needles.iter().all(|needle| line.contains(needle))
        })
        .map(|process| process.pid().as_u32())
        .collect()
}

#[test]
fn tcp_info_and_exec() {
    let world = World::new("");
    let (_host, bound) = world.serve("tcp://127.0.0.1:0");
    assert!(bound.starts_with("tcp://127.0.0.1:"), "{bound}");
    world.exercise(&[&bound], 1);
}

#[test]
fn pipe_info_and_exec() {
    let world = World::new("");
    // A Windows pipe is a name; a Unix socket is a path in a directory only this user can use.
    let endpoint = if cfg!(windows) {
        format!("pipe://bohio-test-{}", std::process::id())
    } else {
        format!("pipe://{}", world.path("host.sock").display())
    };
    let (_host, bound) = world.serve(&endpoint);
    assert!(bound.starts_with("pipe://"), "{bound}");
    world.exercise(&[&bound], 1);
}

#[test]
fn websocket_info_and_exec() {
    let world = World::new("");
    let (_host, bound) = world.serve("ws://127.0.0.1:0");
    assert!(bound.starts_with("ws://127.0.0.1:"), "{bound}");
    world.exercise(&[&bound], 1);
}

#[test]
fn stdio_info_and_exec_and_no_program_is_left() {
    let world = World::new("");
    let marker = world.host.clone();
    let command = format!(
        "\"{}\" serve --stdio --config \"{}\"",
        env!("CARGO_BIN_EXE_bohio"),
        world.host
    );
    world.exercise(&["--command", &command], 1);
    // Each `remote` started a `serve --stdio`: none is left running once the command that started it ended.
    let until = Instant::now() + Duration::from_secs(10);
    while !processes_with(&["--stdio", &marker]).is_empty() && Instant::now() < until {
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        processes_with(&["--stdio", &marker]),
        Vec::<u32>::new(),
        "a `bohio serve --stdio` was left running"
    );
}

#[test]
fn the_process_scan_sees_a_stdio_host_that_is_running() {
    // The control for the survivor check above: it must find one that is there.
    let world = World::new("");
    let host = Proc::start(
        env!("CARGO_BIN_EXE_bohio"),
        &["serve", "--stdio", "--config", &world.host],
        true,
        "warn",
    );
    assert_eq!(
        processes_with(&["--stdio", &world.host]),
        vec![host.child.id()]
    );
}

#[test]
fn stdio_refuses_an_unapproved_key_and_a_listen() {
    let world = World::new("");
    let both = run(&[
        "serve",
        "--stdio",
        "--listen",
        "tcp://127.0.0.1:0",
        "--config",
        &world.host,
    ]);
    assert_eq!(both.code, Some(2), "{}", both.stderr);
}

/// `socat` joining two pseudo-terminals as a null-modem cable, ended when dropped.
#[cfg(unix)]
struct Cable {
    process: Proc,
    ends: Vec<String>,
}

#[cfg(unix)]
impl Cable {
    fn new() -> Option<Self> {
        // Without `socat` there is no cable: the test says so and passes.
        Command::new("socat")
            .arg("-V")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        let mut process = Proc::start(
            "socat",
            &["-d", "-d", "pty,raw,echo=0", "pty,raw,echo=0"],
            false,
            "warn",
        );
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let ends: Vec<String> = text(&process.stderr)
                .lines()
                .filter_map(|line| {
                    line.split_once("PTY is ")
                        .map(|(_, path)| path.trim().to_owned())
                })
                .collect();
            if ends.len() == 2 {
                return Some(Self { process, ends });
            }
            if process.child.try_wait().ok().flatten().is_some() || Instant::now() >= until {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
#[cfg_attr(
    not(unix),
    ignore = "a serial line needs a pseudo-terminal pair (socat) or a null-modem cable; this machine has no COM pair"
)]
fn serial_info_and_exec_over_a_pseudo_terminal_pair() {
    #[cfg(unix)]
    {
        let Some(cable) = Cable::new() else {
            eprintln!(
                "SKIPPED: socat is not installed, so there is no pseudo-terminal pair to use as a serial line"
            );
            return;
        };
        let world = World::new("");
        let host_end = format!("serial://{}?baud=115200", cable.ends[0]);
        let client_end = format!("serial://{}?baud=115200", cable.ends[1]);
        let (_host, bound) = world.serve(&host_end);
        assert_eq!(bound, host_end);
        // The host opens its port a moment after it says where it listens, and drops what arrived before.
        thread::sleep(Duration::from_secs(1));
        world.exercise(&[&client_end], 4);
        drop(cable.process);
    }
}

#[test]
fn status_reports_each_listen_endpoint_by_scheme() {
    let tcp = free_port();
    let ws = free_port();
    let pipe = if cfg!(windows) {
        format!("pipe://bohio-status-{}", std::process::id())
    } else {
        String::new()
    };
    // Unix pipes need a directory of their own, known once the world exists.
    let world = World::new("");
    let pipe = if pipe.is_empty() {
        format!("pipe://{}", world.path("status.sock").display())
    } else {
        pipe
    };
    let serial = if cfg!(windows) {
        "serial://COM98?baud=9600"
    } else {
        "serial:///dev/null?baud=9600"
    };
    let entries = [
        format!("tcp://127.0.0.1:{tcp}"),
        format!("ws://127.0.0.1:{ws}"),
        pipe,
        serial.to_owned(),
        "gopher://nowhere".to_owned(),
    ];
    let listens: String = entries
        .iter()
        .map(|endpoint| {
            format!(
                "[[listen]]\nendpoint = \"{}\"\n",
                endpoint.replace('\\', "\\\\")
            )
        })
        .collect();
    let config = world.path("status.cfg");
    std::fs::write(
        &config,
        format!(
            "[service]\ndata_dir = {}\n{listens}",
            toml_path(&world.path("status-data"))
        ),
    )
    .unwrap();
    let config = config.to_string_lossy().into_owned();
    // The serial entry and the unknown scheme are not served: serve only the three that are.
    let serving = std::fs::read_to_string(&config)
        .unwrap()
        .replace("[[listen]]\nendpoint = \"gopher://nowhere\"\n", "")
        .replace(&format!("[[listen]]\nendpoint = \"{serial}\"\n"), "");
    let served = world.path("served.cfg");
    std::fs::write(&served, serving).unwrap();
    let mut host = Proc::bohio(&["serve", "--config", &served.to_string_lossy()]);
    let _ = host.expect_line("host ");
    let status = run_ok(&["status", "--config", &config]).stdout;
    let line = |endpoint: &str| {
        status
            .lines()
            .find(|line| line.starts_with(&format!("listen: {endpoint} ")))
            .unwrap_or_else(|| panic!("no line for {endpoint} in\n{status}"))
            .to_owned()
    };
    assert!(line(&entries[0]).ends_with(" reachable"), "{status}");
    assert!(line(&entries[1]).ends_with(" reachable"), "{status}");
    assert!(line(&entries[2]).ends_with(" reachable"), "{status}");
    let serial_state = line(serial);
    assert!(
        serial_state.ends_with(if cfg!(windows) {
            " device not checked"
        } else {
            " device present"
        }),
        "{status}"
    );
    assert!(
        line(&entries[4]).ends_with(" not served by this build"),
        "{status}"
    );
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("a port")
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn a_listen_key_for_another_scheme_is_refused_naming_it() {
    let world = World::new("");
    let config = world.path("bad.cfg");
    std::fs::write(
        &config,
        "[[listen]]\nendpoint = \"tcp://127.0.0.1:1\"\nbaud = 9600\n",
    )
    .unwrap();
    let checked = run(&["config", "check", "--config", &config.to_string_lossy()]);
    assert_eq!(checked.code, Some(1), "{}", checked.stdout);
    assert!(checked.stderr.contains("listen.baud"), "{}", checked.stderr);
}

#[test]
fn tls_keys_come_in_a_pair_and_belong_to_the_tls_schemes() {
    let world = World::new("");
    let pair = "tls_cert = \"c.pem\"\ntls_key = \"k.pem\"\n";
    for (entry, key) in [
        (
            "endpoint = \"wss://127.0.0.1:1\"\ntls_cert = \"c.pem\"\n".to_owned(),
            "listen.tls_key",
        ),
        (
            "endpoint = \"wss://127.0.0.1:1\"\ntls_key = \"k.pem\"\n".to_owned(),
            "listen.tls_cert",
        ),
        (
            "endpoint = \"wss://127.0.0.1:1\"\n".to_owned(),
            "listen.tls_cert",
        ),
        (
            format!("endpoint = \"ws://127.0.0.1:1\"\n{pair}"),
            "listen.tls_cert",
        ),
        (
            "endpoint = \"webtransport://127.0.0.1:1\"\ntls_valid_days = 0\n".to_owned(),
            "listen.tls_valid_days",
        ),
        (
            format!("endpoint = \"webtransport://127.0.0.1:1\"\n{pair}tls_valid_days = 3\n"),
            "listen.tls_valid_days",
        ),
        (
            format!("endpoint = \"wss://127.0.0.1:1\"\n{pair}tls_valid_days = 3\n"),
            "listen.tls_valid_days",
        ),
    ] {
        let config = world.path("tls.cfg");
        std::fs::write(&config, format!("[[listen]]\n{entry}")).unwrap();
        let checked = run(&["config", "check", "--config", &config.to_string_lossy()]);
        assert_eq!(checked.code, Some(1), "{entry}: {}", checked.stdout);
        assert!(checked.stderr.contains(key), "{entry}: {}", checked.stderr);
    }
}

#[test]
fn wss_without_a_certificate_and_an_unknown_scheme_are_refused_with_the_reason() {
    let world = World::new("");
    for (endpoint, reason) in [
        ("wss://127.0.0.1:1", "needs tls_cert and tls_key"),
        ("gopher://x", "is not served"),
    ] {
        let attempt = run(&["serve", "--config", &world.host, "--listen", endpoint]);
        assert_eq!(attempt.code, Some(1), "{endpoint}: {}", attempt.stderr);
        assert!(
            attempt.stderr.contains(reason),
            "{endpoint}: {}",
            attempt.stderr
        );
    }
}

/// A certificate authority and a certificate for `127.0.0.1` it issued, generated for one test and
/// written under the world's directory; nothing here is kept.
struct Certificates {
    /// Holds the files, and removes them when the test ends.
    _dir: TempDir,
    /// The authority's certificate: what a client trusts with `--ca`.
    ca: String,
    /// The host's certificate and key, for `tls_cert` and `tls_key`.
    cert: std::path::PathBuf,
    key: std::path::PathBuf,
    /// The host certificate's SHA-256, as `--cert-hash` takes it.
    hash: String,
    /// One line of the key's base64 body, which no log may hold.
    key_body: String,
}

impl Certificates {
    fn new() -> Self {
        Self::starting_in(Duration::ZERO)
    }

    /// As [`Certificates::new`], the host certificate valid only from `delay` on.
    fn starting_in(delay: Duration) -> Self {
        let dir = tempfile::tempdir().expect("a directory");
        let path = |name: &str| dir.path().join(name);
        use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
        use sha2::{Digest, Sha256};
        let authority = || {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(DnType::CommonName, "bohio test authority");
            params
        };
        let authority_key = KeyPair::generate().unwrap();
        let trusted = authority().self_signed(&authority_key).unwrap();
        let signer = Issuer::new(authority(), authority_key);
        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf_params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
        if !delay.is_zero() {
            leaf_params.not_before = time::OffsetDateTime::now_utc() + delay;
        }
        let leaf = leaf_params.signed_by(&leaf_key, &signer).unwrap();
        let ca_path = path("ca.pem");
        std::fs::write(&ca_path, trusted.pem()).unwrap();
        let cert = path("cert.pem");
        std::fs::write(&cert, leaf.pem()).unwrap();
        let key = path("key.pem");
        let key_pem = leaf_key.serialize_pem();
        std::fs::write(&key, &key_pem).unwrap();
        let digest = Sha256::digest(leaf.der());
        Self {
            _dir: dir,
            ca: ca_path.to_string_lossy().into_owned(),
            cert,
            key,
            hash: format!(
                "sha256:{}",
                digest
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
            key_body: key_pem
                .lines()
                .find(|line| !line.starts_with("-----"))
                .unwrap()
                .to_owned(),
        }
    }

    /// A `[[listen]]` entry for `endpoint` with this host certificate.
    fn listen(&self, endpoint: &str) -> String {
        format!(
            "[[listen]]\nendpoint = \"{endpoint}\"\ntls_cert = {}\ntls_key = {}\n",
            toml_path(&self.cert),
            toml_path(&self.key)
        )
    }
}

impl World {
    /// Starts `bohio serve` on the configuration's own `[[listen]]` entries, logging at `log`, and
    /// waits for it to say where it listens.
    fn serve_listed(&self, log: &str) -> (Proc, String) {
        let mut host = Proc::start(
            env!("CARGO_BIN_EXE_bohio"),
            &["serve", "--config", &self.host],
            false,
            log,
        );
        let bound = host.expect_line("listening ");
        (host, bound)
    }
}

#[test]
fn wss_info_and_exec_over_a_generated_certificate() {
    let certificates = Certificates::new();
    let world = World::new(&certificates.listen("wss://127.0.0.1:0"));
    let (host, bound) = world.serve_listed("trace");
    assert!(bound.starts_with("wss://127.0.0.1:"), "{bound}");
    world.exercise(&["--ca", &certificates.ca, &bound], 1);
    // The private key is never logged, at the most verbose level.
    let mut host = host;
    let _ = host.child.kill();
    let done = host.finish();
    assert!(
        !done.stderr.contains(&certificates.key_body)
            && !done.stdout.contains(&certificates.key_body),
        "the host logged its private key"
    );
}

#[test]
fn wss_with_a_certificate_the_client_does_not_trust_fails_as_a_certificate_failure() {
    let certificates = Certificates::new();
    let other = Certificates::new();
    let world = World::new(&certificates.listen("wss://127.0.0.1:0"));
    let (_host, bound) = world.serve_listed("warn");
    // The system's roots do not hold the test authority, and neither does another authority's file.
    for target in [
        vec![bound.as_str()],
        vec!["--ca", other.ca.as_str(), bound.as_str()],
    ] {
        let failed = world.remote(&target, &["info"]);
        assert_eq!(failed.code, Some(1), "{target:?}: {}", failed.stdout);
        assert!(
            failed.stderr.contains("TLS certificate was refused"),
            "{target:?}: {}",
            failed.stderr
        );
    }
}

#[test]
fn wss_is_reached_by_the_certificates_hash_and_not_by_another() {
    let certificates = Certificates::new();
    let other = Certificates::new();
    let world = World::new(&certificates.listen("wss://127.0.0.1:0"));
    let (_host, bound) = world.serve_listed("warn");
    // The link's proof still refuses a key the host has not approved: reaching the host is not trusting it.
    let reached = world.remote(&["--cert-hash", &certificates.hash, &bound], &["info"]);
    assert_eq!(reached.code, Some(1), "{}", reached.stderr);
    assert!(
        !reached.stderr.contains("TLS certificate"),
        "the pinned certificate was refused: {}",
        reached.stderr
    );
    let wrong = world.remote(&["--cert-hash", &other.hash, &bound], &["info"]);
    assert_eq!(wrong.code, Some(1), "{}", wrong.stdout);
    assert!(
        wrong.stderr.contains("TLS certificate was refused"),
        "{}",
        wrong.stderr
    );
}

/// Whether a `remote` run got past the carrier's TLS handshake: the link's proof then refuses the
/// key the host has not approved, which is a different message.
fn tls_accepted(attempt: &Finished) -> bool {
    attempt.code == Some(1) && !attempt.stderr.contains("TLS certificate")
}

#[test]
fn wss_certificate_that_starts_later_is_refused_by_its_hash_until_it_starts() {
    // A certificate made just before the host starts, or a client whose clock is a minute behind: the
    // same hash is refused at first and taken once the client's clock reaches the certificate's start.
    let certificates = Certificates::starting_in(Duration::from_secs(6));
    let world = World::new(&certificates.listen("wss://127.0.0.1:0"));
    let (_host, bound) = world.serve_listed("warn");
    let early = world.remote(&["--cert-hash", &certificates.hash, &bound], &["info"]);
    assert!(!tls_accepted(&early), "{}", early.stderr);
    assert!(
        early.stderr.contains("TLS certificate was refused"),
        "{}",
        early.stderr
    );
    thread::sleep(Duration::from_secs(7));
    let later = world.remote(&["--cert-hash", &certificates.hash, &bound], &["info"]);
    assert!(tls_accepted(&later), "{}", later.stderr);
}

#[test]
fn a_refused_certificate_says_why_and_which_certificate_the_host_presented() {
    let certificates = Certificates::starting_in(Duration::from_secs(600));
    let other = Certificates::new();
    let world = World::new(&certificates.listen("wss://127.0.0.1:0"));
    let (_host, bound) = world.serve_listed("warn");
    // Pinned, but not valid yet: the reason is the clock, and the way out is named.
    let early = world.remote(&["--cert-hash", &certificates.hash, &bound], &["info"]);
    assert!(
        early.stderr.contains("not valid until") && early.stderr.contains("--clock-skew"),
        "{}",
        early.stderr
    );
    // Another hash: the message holds the hash the host does present, so a stale pin shows.
    let stale = world.remote(&["--cert-hash", &other.hash, &bound], &["info"]);
    assert!(
        stale.stderr.contains(&certificates.hash),
        "the presented certificate is not named: {}",
        stale.stderr
    );
}

#[test]
fn a_clock_skew_the_client_allows_takes_a_certificate_that_starts_within_it() {
    let certificates = Certificates::starting_in(Duration::from_secs(600));
    let world = World::new(&certificates.listen("wss://127.0.0.1:0"));
    let (_host, bound) = world.serve_listed("warn");
    let pin = ["--cert-hash", certificates.hash.as_str()];
    let args = |skew: &'static str| [pin[0], pin[1], "--clock-skew", skew, bound.as_str()];
    let within = world.remote(&args("15m"), &["info"]);
    assert!(tls_accepted(&within), "{}", within.stderr);
    let short = world.remote(&args("1m"), &["info"]);
    assert!(!tls_accepted(&short), "{}", short.stderr);
}

/// A hash in the right form that no certificate has.
const NO_CERTIFICATE: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

#[test]
fn webtransport_info_read_and_exec_over_a_certificate_pinned_by_hash() {
    let world = World::new("");
    let (mut host, bound) = world.serve("webtransport://127.0.0.1:0");
    assert!(bound.starts_with("webtransport://127.0.0.1:"), "{bound}");
    // The host made the certificate and says its hash: what a browser's `serverCertificateHashes` takes.
    let line = host.expect_line("certificate ");
    let hash = line.split(' ').next().unwrap().to_owned();
    assert!(line.ends_with(&format!(" for {bound}")), "{line}");
    world.exercise(&["--cert-hash", &hash, &bound], 1);
}

#[test]
fn webtransport_with_the_wrong_hash_fails_as_a_certificate_failure() {
    let world = World::new("");
    let (_host, bound) = world.serve("webtransport://127.0.0.1:0");
    let failed = world.remote(&["--cert-hash", NO_CERTIFICATE, &bound], &["info"]);
    assert_eq!(failed.code, Some(1), "{}", failed.stdout);
    assert!(
        failed.stderr.contains("TLS certificate was refused"),
        "{}",
        failed.stderr
    );
    // No pin and no CA: the system's roots do not hold the host's own certificate.
    let unknown = world.remote(&[&bound], &["info"]);
    assert_eq!(unknown.code, Some(1), "{}", unknown.stdout);
    assert!(
        unknown.stderr.contains("TLS certificate was refused"),
        "{}",
        unknown.stderr
    );
}

#[test]
fn webtransport_takes_the_longest_validity_a_browser_pins() {
    let world =
        World::new("[[listen]]\nendpoint = \"webtransport://127.0.0.1:0\"\ntls_valid_days = 14\n");
    let (mut host, bound) = world.serve_listed("warn");
    let hash = host
        .expect_line("certificate ")
        .split(' ')
        .next()
        .unwrap()
        .to_owned();
    // The proof refuses a key the host has not approved; the certificate itself is taken.
    let reached = world.remote(&["--cert-hash", &hash, &bound], &["info"]);
    assert_eq!(reached.code, Some(1), "{}", reached.stderr);
    assert!(
        !reached.stderr.contains("TLS certificate"),
        "{}",
        reached.stderr
    );
}

/// What a host's log says, once it has been stopped.
fn stopped_log(mut host: Proc) -> String {
    let _ = host.child.kill();
    let done = host.finish();
    format!("{}{}", done.stdout, done.stderr)
}

#[test]
fn a_validity_beyond_what_a_browser_pins_is_taken_logged_once_and_pinned_by_hash() {
    let world =
        World::new("[[listen]]\nendpoint = \"webtransport://127.0.0.1:0\"\ntls_valid_days = 400\n");
    let (mut host, bound) = world.serve_listed("warn");
    let hash = host
        .expect_line("certificate ")
        .split(' ')
        .next()
        .unwrap()
        .to_owned();
    world.exercise(&["--cert-hash", &hash, &bound], 1);
    let log = stopped_log(host);
    assert_eq!(log.matches("a browser will not accept").count(), 1, "{log}");
    assert!(log.contains("400 days"), "{log}");
}

#[test]
fn a_certificate_from_files_a_browser_would_not_pin_is_logged_and_still_pinned_by_hash() {
    let certificates = Certificates::new();
    let world = World::new(&certificates.listen("webtransport://127.0.0.1:0"));
    let (host, bound) = world.serve_listed("warn");
    world.exercise(&["--cert-hash", &certificates.hash, &bound], 1);
    let log = stopped_log(host);
    assert_eq!(log.matches("a browser will not accept").count(), 1, "{log}");
}

#[test]
fn webtransport_over_a_certificate_from_files_is_trusted_through_its_authority() {
    let certificates = Certificates::new();
    let world = World::new(&certificates.listen("webtransport://127.0.0.1:0"));
    let (mut host, bound) = world.serve_listed("warn");
    let line = host.expect_line("certificate ");
    assert_eq!(line, format!("{} for {bound}", certificates.hash));
    world.exercise(&["--ca", &certificates.ca, &bound], 1);
}

/// This host's address on a route to the outside, or `None` when it has none (no network is no failure here).
fn routed_address() -> Option<std::net::IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("192.0.2.1:9").ok()?;
    let address = probe.local_addr().ok()?.ip();
    (!address.is_loopback() && !address.is_unspecified()).then_some(address)
}

/// A host listening on `ws_listen` with the client approved, and a UDP port nothing answers on.
fn fallback_world(ws_listen: &str) -> (World, Proc, String, std::net::UdpSocket) {
    let world = World::new(&format!("[[listen]]\nendpoint = \"{ws_listen}\"\n"));
    let (mut host, bound) = world.serve_listed("warn");
    let port = bound.rsplit(':').next().unwrap().to_owned();
    assert_eq!(
        world
            .remote(&[&bound.replace("0.0.0.0", "127.0.0.1")], &["info"])
            .code,
        Some(1)
    );
    run_ok(&[
        "peer",
        "approve",
        &world.client_id,
        "--config",
        &world.host,
        "--allow",
        "info",
    ]);
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let _ = &mut host;
    (world, host, port, silent)
}

#[test]
fn a_fallback_on_another_host_is_dialled_when_the_operator_names_it() {
    // 127.0.0.2 is another host name than the WebTransport endpoint's 127.0.0.1; macOS has no such address.
    if std::net::UdpSocket::bind("127.0.0.2:0").is_err() {
        println!("skipped: 127.0.0.2 cannot be bound here");
        return;
    }
    let (world, _host, port, silent) = fallback_world("ws://127.0.0.2:0");
    let blocked = format!("webtransport://{}", silent.local_addr().unwrap());
    let ws = format!("ws://127.0.0.2:{port}");
    let fell_back = world.remote(
        &["--fallback", &ws, &blocked],
        &["info", "--format", "json"],
    );
    assert_eq!(
        fell_back.code,
        Some(0),
        "stdout: {}\nstderr: {}",
        fell_back.stdout,
        fell_back.stderr
    );
    assert!(
        fell_back.stdout.contains(&world.host_id),
        "{}",
        fell_back.stdout
    );
}

#[test]
fn a_plain_fallback_beyond_loopback_is_dialled_when_the_operator_names_it() {
    let Some(address) = routed_address() else {
        println!("skipped: this host has no routed address");
        return;
    };
    let (world, _host, port, silent) = fallback_world("ws://0.0.0.0:0");
    // The WebTransport endpoint names a different host than the fallback, which is plain on a routed address.
    let blocked = format!("webtransport://{}", silent.local_addr().unwrap());
    let ws = format!("ws://{address}:{port}");
    let fell_back = world.remote(
        &["--fallback", &ws, &blocked],
        &["info", "--format", "json"],
    );
    assert_eq!(
        fell_back.code,
        Some(0),
        "stdout: {}\nstderr: {}",
        fell_back.stdout,
        fell_back.stderr
    );
    assert!(
        fell_back.stdout.contains(&world.host_id),
        "{}",
        fell_back.stdout
    );
}

#[test]
fn a_blocked_udp_path_falls_back_to_the_websocket_endpoint_and_a_refused_certificate_does_not() {
    let world = World::new(
        "[[listen]]\nendpoint = \"webtransport://127.0.0.1:0\"\n[[listen]]\nendpoint = \"ws://127.0.0.1:0\"\n",
    );
    let (mut host, _) = world.serve_listed("warn");
    let wt = host.expect_line("listening webtransport://");
    let ws = format!("ws://{}", host.expect_line("listening ws://"));
    let hash = {
        let line = host.expect_line("certificate ");
        line.split(' ').next().unwrap().to_owned()
    };
    // The host learns the key when it is first refused; then it can be approved.
    assert_eq!(world.remote(&[&ws], &["info"]).code, Some(1));
    run_ok(&[
        "peer",
        "approve",
        &world.client_id,
        "--config",
        &world.host,
        "--allow",
        "info",
    ]);
    // Nothing answers on this UDP port, which is what a path that blocks QUIC looks like.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let blocked = format!("webtransport://{}", silent.local_addr().unwrap());
    let started = Instant::now();
    let fell_back = world.remote(
        &["--fallback", &ws, &blocked],
        &["info", "--format", "json"],
    );
    assert_eq!(
        fell_back.code,
        Some(0),
        "stdout: {}\nstderr: {}",
        fell_back.stdout,
        fell_back.stderr
    );
    assert!(
        fell_back
            .stdout
            .contains(&format!("\"host_id\":\"{}\"", world.host_id)),
        "{}",
        fell_back.stdout
    );
    // It waited for the QUIC deadline first (`proof_timeout`, 5 s here): the fallback is the second try.
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    // Without a fallback the same blocked path fails.
    let alone = world.remote(&[&blocked], &["info"]);
    assert_eq!(alone.code, Some(1), "{}", alone.stdout);

    // The path works but its certificate is refused: nothing is dialled on the WebSocket endpoint.
    let live = format!("webtransport://{wt}");
    let refused = world.remote(
        &["--cert-hash", NO_CERTIFICATE, "--fallback", &ws, &live],
        &["info"],
    );
    assert_eq!(refused.code, Some(1), "{}", refused.stdout);
    assert!(
        refused.stderr.contains("TLS certificate was refused"),
        "{}",
        refused.stderr
    );
    // The control: the right hash on the same pair reaches the WebTransport host itself.
    let pinned = world.remote(
        &["--cert-hash", &hash, "--fallback", &ws, &live],
        &["info", "--format", "json"],
    );
    assert_eq!(pinned.code, Some(0), "{}", pinned.stderr);
}

#[test]
fn a_made_webtransport_certificate_takes_the_backdate_the_entry_gives() {
    let world = World::new(
        "[[listen]]\nendpoint = \"webtransport://127.0.0.1:0\"\ntls_backdate = \"0s\"\n",
    );
    let (mut host, bound) = world.serve_listed("warn");
    let hash = host
        .expect_line("certificate ")
        .split(' ')
        .next()
        .unwrap()
        .to_owned();
    // Starting now, on the same clock, it is valid.
    let reached = world.remote(&["--cert-hash", &hash, &bound], &["info"]);
    assert!(tls_accepted(&reached), "{}", reached.stderr);
}
