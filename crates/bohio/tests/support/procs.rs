//! `bohio` child processes for the tests that run two nodes: each has its output read on threads, and is killed
//! when its handle drops.

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A wait long enough for any exchange on this machine, short enough to fail a hung test.
pub const DEADLINE: Duration = Duration::from_secs(60);

/// A child process whose output is read on threads and which is killed when dropped.
pub struct Proc {
    child: Child,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    readers: Vec<JoinHandle<()>>,
}

/// What a finished child left.
pub struct Finished {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
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
    /// Starts `bohio` with `args` and `log` as its log filter.
    pub fn start(args: &[&str], log: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bohio"));
        command
            .args(args)
            .env_remove("BOHIO_CONFIG")
            .env("BOHIO_LOG", log)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("the binary starts");
        let (stdout, out_reader) = collect(child.stdout.take().unwrap());
        let (stderr, err_reader) = collect(child.stderr.take().unwrap());
        Self {
            child,
            stdout,
            stderr,
            readers: vec![out_reader, err_reader],
        }
    }

    pub fn stdout(&self) -> String {
        text(&self.stdout)
    }

    pub fn stderr(&self) -> String {
        text(&self.stderr)
    }

    /// The rest of the first standard-output line that starts with `prefix`, waiting for it.
    pub fn expect_line(&mut self, prefix: &str) -> String {
        let until = Instant::now() + DEADLINE;
        loop {
            if let Some(rest) = text(&self.stdout)
                .lines()
                .find_map(|line| line.strip_prefix(prefix).map(|rest| rest.trim().to_owned()))
            {
                return rest;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
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
                "no `{prefix}` line within the deadline\nstdout: {}\nstderr: {}",
                text(&self.stdout),
                text(&self.stderr)
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// How many times `needle` is in what the child wrote to standard error.
    pub fn stderr_count(&self, needle: &str) -> usize {
        text(&self.stderr).matches(needle).count()
    }

    /// Waits until standard error holds `needle` at least `times` times, failing at the deadline.
    pub fn wait_stderr(&self, needle: &str, times: usize, within: Duration) {
        let until = Instant::now() + within;
        while self.stderr_count(needle) < times {
            assert!(
                Instant::now() < until,
                "`{needle}` was not written {times} time(s) within {within:?}\nstderr: {}",
                text(&self.stderr)
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Kills the child and waits for it, so its port is free.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Waits for the child to end, killing it and failing at the deadline.
    pub fn finish(mut self) -> Finished {
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
        self.kill();
    }
}

pub fn run(args: &[&str]) -> Finished {
    Proc::start(args, "warn").finish()
}

pub fn run_ok(args: &[&str]) -> Finished {
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

/// Whether something accepts connections at `address`, refusing a socket that connected to itself.
pub fn reachable(address: SocketAddr) -> bool {
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

/// A TOML literal string for `path`.
pub fn toml_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// A node of a test with two or more: its configuration file and data directory under `dir`, and its id.
pub struct Node {
    pub name: String,
    pub dir: PathBuf,
    pub config: PathBuf,
    pub id: String,
}

impl Node {
    /// A node named `name`, with a configuration of defaults and its identity made.
    pub fn new(dir: &Path, name: &str) -> Self {
        let node = Self {
            name: name.to_owned(),
            dir: dir.to_owned(),
            config: dir.join(format!("{name}.cfg")),
            id: String::new(),
        };
        node.write("");
        let id = run_ok(&["id", "--config", &node.config()])
            .stdout
            .trim()
            .to_owned();
        Self { id, ..node }
    }

    pub fn data_dir(&self) -> PathBuf {
        self.dir.join(format!("{}-data", self.name))
    }

    pub fn config(&self) -> String {
        self.config.to_string_lossy().into_owned()
    }

    /// Writes the configuration: this node's `[service]` and a `[link]` of a one-second re-check with
    /// `link`, then `extra` (the rest of the file).
    pub fn write_with(&self, link: &str, extra: &str) {
        std::fs::write(
            &self.config,
            format!(
                "[service]\nname = \"{}\"\ndata_dir = {}\n[link]\nrecheck = \"1s\"\n{link}\n{extra}\n",
                self.name,
                toml_path(&self.data_dir())
            ),
        )
        .expect("the configuration is written");
    }

    pub fn write(&self, extra: &str) {
        self.write_with("", extra);
    }

    /// `bohio serve` with this node's configuration and `listen` arguments, waiting for its first
    /// `listening` line when it listens; the address it listens on is returned beside it.
    pub fn serve(&self, listen: Option<&str>, log: &str) -> (Proc, Option<(SocketAddr, String)>) {
        let config = self.config();
        let mut args = vec!["serve", "--config", &config];
        if let Some(listen) = listen {
            args.extend_from_slice(&["--listen", listen]);
        }
        let mut process = Proc::start(&args, log);
        let listening = listen.map(|_| {
            let endpoint = process.expect_line("listening ");
            let address: SocketAddr = endpoint
                .strip_prefix("tcp://")
                .expect("a tcp endpoint")
                .parse()
                .expect("an address");
            assert!(reachable(address), "the node does not answer on {address}");
            (address, endpoint)
        });
        (process, listening)
    }

    /// `bohio peer list` for this node.
    pub fn peers(&self) -> String {
        run_ok(&["peer", "list", "--config", &self.config()]).stdout
    }

    /// A `[peer."<id>"]` table that dials `endpoint` with a quick retry, for `grant` lines.
    pub fn peer_table(id: &str, endpoint: Option<&str>, grant: &str, dial: &str) -> String {
        let mut text = format!("[peer.\"{id}\"]\n{grant}\n");
        if let Some(endpoint) = endpoint {
            text.push_str(&format!("endpoint = \"{endpoint}\"\n"));
            text.push_str(&format!("[peer.\"{id}\".dial]\n{dial}\n"));
        }
        text
    }
}
