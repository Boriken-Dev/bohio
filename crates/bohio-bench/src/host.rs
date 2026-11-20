//! A `bohio serve` child, its trust set up only through `bohio pair`, `join` and `peer approve`, and
//! the CPU time of its process tree.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// How long a child gets to say what it is listening on.
const START: Duration = Duration::from_secs(60);

/// A child process whose standard output is read line by line, and which is killed when dropped.
pub struct Proc {
    child: Child,
    lines: Receiver<String>,
}

impl Proc {
    /// Starts `argv` (the program first) with `env` set.
    ///
    /// # Errors
    ///
    /// The error of starting the program.
    pub fn start(argv: &[String], env: &[(&str, &str)]) -> Result<Self, String> {
        let (program, args) = argv.split_first().ok_or("an empty command")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .env_remove("BOHIO_CONFIG")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("{program}: {error}"))?;
        let stdout = child.stdout.take().ok_or("no standard output")?;
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self { child, lines })
    }

    /// The rest of the first line that starts with `prefix`.
    ///
    /// # Errors
    ///
    /// When the child ends or says nothing like it within a minute.
    pub fn expect(&mut self, prefix: &str) -> Result<String, String> {
        let until = Instant::now() + START;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if let Some(rest) = line.strip_prefix(prefix) {
                        return Ok(rest.trim().to_owned());
                    }
                }
                Err(_) => return Err(format!("no `{prefix}` line from the child")),
            }
        }
    }

    /// The child's process id.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Waits for the child to end, killing it after `limit`; its exit code.
    pub fn finish(mut self, limit: Duration) -> Option<i32> {
        let until = Instant::now() + limit;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            if Instant::now() >= until {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs `argv` to its end and returns its standard output; an error with the exit code otherwise.
///
/// # Errors
///
/// A command that does not start or does not exit 0.
pub fn run(argv: &[String]) -> Result<String, String> {
    let (program, args) = argv.split_first().ok_or("an empty command")?;
    let output = Command::new(program)
        .args(args)
        .env_remove("BOHIO_CONFIG")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{argv:?} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Reads the CPU time of a process and everything it started.
pub struct Cpu {
    system: System,
    root: Pid,
}

impl Cpu {
    /// Watches the process `pid` and its descendants.
    pub fn of(pid: u32) -> Self {
        Self {
            system: System::new(),
            root: Pid::from_u32(pid),
        }
    }

    /// User plus system milliseconds the tree has used so far.
    pub fn used_ms(&mut self) -> u64 {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu(),
        );
        let mut family = vec![self.root];
        let mut index = 0;
        while index < family.len() {
            let parent = family[index];
            for (pid, process) in self.system.processes() {
                if process.parent() == Some(parent) && !family.contains(pid) {
                    family.push(*pid);
                }
            }
            index += 1;
        }
        family
            .iter()
            .filter_map(|pid| self.system.process(*pid))
            .map(sysinfo::Process::accumulated_cpu_time)
            .sum()
    }
}

/// `[section]` keys by section, each value as TOML text.
pub type Sections = BTreeMap<String, BTreeMap<String, String>>;

/// A configuration file in `dir`: a service name, a data directory and extra `[section]` keys.
fn write_config(dir: &Path, name: &str, extra: &Sections) -> Result<PathBuf, String> {
    let data = dir.join(format!("{name}-data"));
    let mut text = format!(
        "[service]\nname = \"{name}\"\ndata_dir = '{}'\n",
        data.display()
    );
    for (section, keys) in extra {
        text.push_str(&format!("[{section}]\n"));
        for (key, value) in keys {
            text.push_str(&format!("{key} = {value}\n"));
        }
    }
    let path = dir.join(format!("{name}.cfg"));
    std::fs::write(&path, text).map_err(|error| error.to_string())?;
    Ok(path)
}

/// A host the harness started and a client approved on it.
pub struct Local {
    /// The host process.
    pub host: Proc,
    /// What the host listens on, `tcp://address:port`.
    pub endpoint: String,
    /// The client's configuration file.
    pub client_config: PathBuf,
    /// The client's data directory.
    pub client_data: PathBuf,
}

/// What a local setup needs.
pub struct Setup<'a> {
    /// The `bohio` to run.
    pub bohio: &'a Path,
    /// A scratch directory for both sides' data.
    pub dir: &'a Path,
    /// A command prefix for the host's processes (for example one that enters a network namespace).
    pub prefix: &'a [String],
    /// The address the host listens on.
    pub listen_ip: &'a str,
    /// The directory the client may read and write.
    pub root: &'a Path,
    /// The program the client may run.
    pub program: &'a Path,
    /// `[link]` and `[filesystem]` keys, as TOML values, for the host.
    pub extra: &'a Sections,
}

/// Starts a host, pairs a client with it and approves the client for info, exec and files under the
/// root, through the `bohio` commands only.
///
/// # Errors
///
/// The first command that fails.
pub fn start(setup: &Setup<'_>) -> Result<Local, String> {
    let bohio = setup.bohio.to_string_lossy().into_owned();
    let host_config = write_config(setup.dir, "bench-host", setup.extra)?;
    let client_config = write_config(setup.dir, "bench-client", &BTreeMap::new())?;
    let host_config_text = host_config.to_string_lossy().into_owned();
    let client_config_text = client_config.to_string_lossy().into_owned();
    let with_prefix =
        |args: Vec<String>| -> Vec<String> { setup.prefix.iter().cloned().chain(args).collect() };
    let words =
        |list: &[&str]| -> Vec<String> { list.iter().map(|word| (*word).to_owned()).collect() };

    let client_id = run(&{
        let mut argv = vec![bohio.clone()];
        argv.extend(words(&["id", "--config", &client_config_text]));
        argv
    })?
    .trim()
    .to_owned();

    // Pair: the host shows a PIN, the client joins with it.
    let mut door = Proc::start(
        &with_prefix({
            let mut argv = vec![bohio.clone()];
            argv.extend(words(&[
                "pair",
                "--config",
                &host_config_text,
                "--listen",
                &format!("tcp://{}:0", setup.listen_ip),
                "--timeout",
                "120s",
            ]));
            argv
        }),
        &[],
    )?;
    let door_endpoint = door.expect("pairing on ")?;
    let pin = door.expect("pin ")?;
    run(&{
        let mut argv = vec![bohio.clone()];
        argv.extend(words(&[
            "join",
            &door_endpoint,
            "--pin",
            &pin,
            "--config",
            &client_config_text,
        ]));
        argv
    })?;
    door.finish(Duration::from_secs(30));

    let root = setup.root.to_string_lossy().into_owned();
    let program = setup.program.to_string_lossy().into_owned();
    run(&{
        let mut argv = vec![bohio.clone()];
        argv.extend(words(&[
            "peer",
            "approve",
            &client_id,
            "--config",
            &host_config_text,
            "--allow",
            "info,exec,fs.read,fs.write",
            "--fs-root",
            &root,
            "--program",
            &program,
        ]));
        argv
    })?;

    let mut host = Proc::start(
        &with_prefix({
            let mut argv = vec![bohio.clone()];
            argv.extend(words(&[
                "serve",
                "--config",
                &host_config_text,
                "--listen",
                &format!("tcp://{}:0", setup.listen_ip),
            ]));
            argv
        }),
        &[],
    )?;
    let endpoint = host.expect("listening ")?;
    host.expect("host ")?;
    Ok(Local {
        host,
        endpoint,
        client_config,
        client_data: setup.dir.join("bench-client-data"),
    })
}
