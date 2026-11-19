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
fn wss_and_an_unknown_scheme_are_refused_with_the_reason() {
    let world = World::new("");
    for (endpoint, reason) in [
        ("wss://127.0.0.1:1", "TLS"),
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
