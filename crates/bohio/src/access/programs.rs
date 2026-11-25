//! Provider programs (`[[providers.program]]`): children that serve Bohío kinds over guatiao's remote protocol on their
//! standard input and output.
//!
//! A program shares nothing with the node but those two pipes, so it is written in any language, built any way and linked
//! to any C library; one that crashes ends only its own link. The node starts each program as a child through a
//! [`RemoteSource`], so what it offers is registered as a provider exactly as a mapped library's is. A program that exits is
//! reported `failed` with how it ended and is started again on the next use, within its `restart_limit`.
//!
//! Every check is a default the operator may weaken: a program file or its directory that another account may write is
//! refused while `[providers] check_access` holds, and run with a `non-default:` line in the log when it does not.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bohio_api::guatiao::library::Registry;
use bohio_proto::hash::Algorithm;
use bohio_transfer::hash::digest_reader;
use guatiao_source_remote::transport::stream::{StreamReader, StreamWriter};
use guatiao_source_remote::transport::{Connect, Connection, FrameRead, FrameWrite};
use guatiao_source_remote::{LinkOptions, RemoteSource, Trust};

use crate::config::{ProgramConfig, ProvidersConfig};
use crate::ownership;

/// What became of one program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    /// Started, and its providers are registered.
    Running,
    /// Could not be started, was refused, or exited and is not (yet) started again.
    Failed,
}

impl ProgramState {
    /// The word the log and `bohio providers` use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Failed => "failed",
        }
    }
}

/// One `[[providers.program]]`, as the node knows it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    /// The entry's name.
    pub name: String,
    /// The program file, as the entry named it.
    pub path: PathBuf,
    /// What became of it.
    pub state: ProgramState,
    /// Why, or what it offers.
    pub detail: String,
    /// `sha256:` and the digest of the file's bytes, when it was read.
    pub digest: Option<String>,
    /// Times it was started again after exiting.
    pub restarts: u32,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One run of a program: its process, and how it ended.
#[derive(Default)]
struct Run {
    process: Mutex<Option<Child>>,
    exit: Mutex<Option<ExitStatus>>,
    /// The program's output ended: it is gone or going, and the link with it, though its exit may not be visible yet.
    closed: AtomicBool,
}

impl Run {
    /// How the process ended, once it has.
    fn poll(&self) -> Option<ExitStatus> {
        let mut process = lock(&self.process);
        if let Some(child) = process.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            *lock(&self.exit) = Some(status);
            *process = None;
        }
        *lock(&self.exit)
    }

    /// How the process ended, once its output has: given `grace` to be seen to exit, else ended.
    fn reap(&self, grace: Duration) -> Option<ExitStatus> {
        if let Some(status) = self.poll() {
            return Some(status);
        }
        if !self.closed.load(Ordering::Acquire) {
            return None;
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if let Some(status) = self.poll() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.kill();
        *lock(&self.exit)
    }

    fn kill(&self) {
        let mut process = lock(&self.process);
        if let Some(child) = process.as_mut() {
            let _ = child.kill();
            if let Ok(status) = child.wait() {
                *lock(&self.exit) = Some(status);
            }
        }
        *process = None;
    }
}

/// Reads a child's output, tells the watchdog when the first bytes came and the run when the output ended.
struct Seen<R> {
    inner: R,
    first: Option<Sender<()>>,
    run: Arc<Run>,
}

impl<R: Read> Read for Seen<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = match self.inner.read(buf) {
            Ok(count) => count,
            Err(error) => {
                self.run.closed.store(true, Ordering::Release);
                return Err(error);
            }
        };
        if count > 0 {
            self.first = None;
        } else {
            self.run.closed.store(true, Ordering::Release);
        }
        Ok(count)
    }
}

/// The child's input, and the child, ended together.
struct Writer {
    stdin: Option<StreamWriter<ChildStdin>>,
    run: Arc<Run>,
    grace: Duration,
}

impl Writer {
    fn input(&mut self) -> io::Result<&mut StreamWriter<ChildStdin>> {
        self.stdin
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

impl FrameWrite for Writer {
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.input()?.write_frame(frame)
    }

    fn write_frame_parts(&mut self, head: &[u8], tail: &[u8]) -> io::Result<()> {
        self.input()?.write_frame_parts(head, tail)
    }

    fn write_frames(&mut self, frames: &[&[u8]]) -> io::Result<()> {
        self.input()?.write_frames(frames)
    }

    fn shutdown(&mut self) {
        // Closing its input asks the program to leave; one that does not within its grace is ended.
        self.stdin = None;
        let deadline = Instant::now() + self.grace;
        while Instant::now() < deadline {
            if self.run.poll().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.run.kill();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.stdin.is_some() || self.run.poll().is_none() {
            self.shutdown();
        }
    }
}

/// Starts the program again at each connection, and remembers the run each made.
struct Start {
    command: Vec<String>,
    env: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
    grace: Duration,
    first_message: Duration,
    current: Arc<Mutex<Arc<Run>>>,
}

impl Connect for Start {
    fn connect(&self) -> io::Result<Connection> {
        let mut command = Command::new(&self.command[0]);
        command
            .args(&self.command[1..])
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window of its own: the node may be a service.
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let run = Arc::new(Run {
            process: Mutex::new(Some(child)),
            ..Run::default()
        });
        *lock(&self.current) = Arc::clone(&run);
        // A program that never sends its first message would hold the node's start for ever: nothing bounds a read of a
        // child's pipe, so a watchdog ends the program instead.
        let (first, heard) = mpsc::channel::<()>();
        let watched = Arc::clone(&run);
        let wait = self.first_message;
        std::thread::spawn(move || {
            if let Err(RecvTimeoutError::Timeout) = heard.recv_timeout(wait) {
                watched.kill();
            }
        });
        let reader: Box<dyn FrameRead> = Box::new(StreamReader::new(Seen {
            inner: stdout,
            first: Some(first),
            run: Arc::clone(&run),
        }));
        Ok(Connection {
            reader,
            writer: Box::new(Writer {
                stdin: Some(StreamWriter::new(stdin)),
                run,
                grace: self.grace,
            }),
            peer: format!("program {}", self.command[0]),
            closer: None,
        })
    }

    fn describe(&self) -> String {
        format!("program {}", self.command[0])
    }
}

/// What is known of one entry beyond what it was configured with.
struct Status {
    state: ProgramState,
    detail: String,
    /// When each start inside the restart window began.
    starts: Vec<Instant>,
    restarts: u32,
    /// When it was last seen to have ended; a failed entry that can be started again is waiting from then.
    ended: Option<Instant>,
    /// The ids of the providers it offered when it last ran.
    providers: Vec<String>,
}

struct Entry {
    name: String,
    source_name: String,
    config: ProgramConfig,
    file: PathBuf,
    digest: Option<String>,
    /// `None` when the entry was refused before anything started.
    source: Option<RemoteSource>,
    current: Arc<Mutex<Arc<Run>>>,
    status: Mutex<Status>,
}

/// The program entries of a node.
#[derive(Default)]
pub(super) struct Programs {
    entries: Vec<Entry>,
    /// Held while programs are started again, so two lookups do not start one twice.
    restarting: Mutex<()>,
}

impl core::fmt::Debug for Programs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Programs")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl Entry {
    fn refused(name: String, config: &ProgramConfig, digest: Option<String>, why: String) -> Self {
        let file = PathBuf::from(&config.command[0]);
        let entry = Self {
            source_name: format!("program:{name}"),
            name,
            config: config.clone(),
            file,
            digest,
            source: None,
            current: Arc::default(),
            status: Mutex::new(Status {
                state: ProgramState::Failed,
                detail: why,
                starts: Vec::new(),
                restarts: 0,
                ended: None,
                providers: Vec::new(),
            }),
        };
        entry.log();
        entry
    }

    fn log(&self) {
        let status = lock(&self.status);
        let line = format!(
            "provider program {} {}: {}: {} ({})",
            self.name,
            self.file.display(),
            status.state.name(),
            status.detail,
            self.digest.as_deref().unwrap_or("no digest")
        );
        match status.state {
            ProgramState::Running => tracing::info!("{line}"),
            ProgramState::Failed => tracing::warn!("{line}"),
        }
    }

    fn run(&self) -> Arc<Run> {
        Arc::clone(&lock(&self.current))
    }

    /// Notes that the program ended on its own, if it did.
    fn observe(&self) {
        if self.source.is_none() {
            return;
        }
        let mut status = lock(&self.status);
        if status.state != ProgramState::Running {
            return;
        }
        if let Some(exit) = self.run().reap(self.config.stop_grace.0) {
            status.state = ProgramState::Failed;
            status.detail = format!("the program exited ({exit})");
            status.ended = Some(Instant::now());
            drop(status);
            self.log();
        }
    }

    /// Whether the program may be started again now.
    fn restartable(&self, now: Instant) -> bool {
        let mut status = lock(&self.status);
        if self.source.is_none() || status.state != ProgramState::Failed {
            return false;
        }
        let Some(ended) = status.ended else {
            return false;
        };
        let window = self.config.restart_window.0;
        status
            .starts
            .retain(|start| now.saturating_duration_since(*start) < window);
        let limit = usize::try_from(self.config.restart_limit).unwrap_or(usize::MAX);
        status.starts.len() < limit
            && now.saturating_duration_since(ended) >= self.config.restart_delay.0
    }
}

impl Programs {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The right to start programs again, or `None` while another lookup is doing it.
    pub(super) fn begin_restart(&self) -> Option<MutexGuard<'_, ()>> {
        self.restarting.try_lock().ok()
    }

    /// Adds each entry of `config` to `registry` as a source and connects them, which starts the programs.
    pub(super) fn start(registry: &mut Registry, config: &ProvidersConfig) -> Self {
        let mut programs = Self::default();
        if config.program.is_empty() {
            return programs;
        }
        if !config.check_access && config.paths.is_empty() {
            tracing::warn!(
                "non-default: [providers] check_access is false: who owns and who may write a provider program and its directory is not checked: whoever can write there chooses the code this node runs"
            );
        }
        let mut checked = ownership::Checked::new();
        for (index, program) in config.program.iter().enumerate() {
            let name = program.label(index);
            let entry = Entry::begin(registry, &name, program, config.check_access, &mut checked);
            programs.entries.push(entry);
        }
        programs
    }

    /// Every entry as it is now: one that ended on its own is `failed`.
    pub(super) fn report(&self) -> Vec<Program> {
        self.entries
            .iter()
            .map(|entry| {
                entry.observe();
                let status = lock(&entry.status);
                Program {
                    name: entry.name.clone(),
                    path: entry.file.clone(),
                    state: status.state,
                    detail: status.detail.clone(),
                    digest: entry.digest.clone(),
                    restarts: status.restarts,
                }
            })
            .collect()
    }

    /// Whether some program ended and may be started again now.
    pub(super) fn wants_restart(&self) -> bool {
        let now = Instant::now();
        self.entries.iter().any(|entry| {
            entry.observe();
            entry.restartable(now)
        })
    }

    /// Starts again every program that ended and may be, and returns the ids of the providers they offer.
    pub(super) fn restart(&self, registry: &mut Registry) -> Vec<String> {
        let now = Instant::now();
        let due: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|entry| entry.restartable(now))
            .collect();
        if due.is_empty() {
            return Vec::new();
        }
        for entry in &due {
            let mut status = lock(&entry.status);
            status.starts.push(now);
            status.restarts += 1;
        }
        let before = ids_of(registry);
        let refreshed = registry.refresh();
        let mut offered = Vec::new();
        for entry in due {
            let refusals: Vec<String> = refreshed
                .refused
                .iter()
                .filter(|refused| refused.source == entry.source_name)
                .map(|refused| refused.reason.clone())
                .collect();
            entry.settle(registry, &before, &refusals, &[]);
            offered.extend(lock(&entry.status).providers.clone());
        }
        offered
    }
}

fn ids_of(registry: &Registry) -> Vec<String> {
    registry
        .all()
        .iter()
        .map(|provider| provider.id().to_owned())
        .collect()
}

impl Entry {
    /// Checks, adds and starts one entry.
    fn begin(
        registry: &mut Registry,
        name: &str,
        config: &ProgramConfig,
        check_access: bool,
        checked: &mut ownership::Checked,
    ) -> Self {
        let file = PathBuf::from(&config.command[0]);
        if !file.is_file() {
            return Self::refused(
                name.to_owned(),
                config,
                None,
                "names no file (the program is not looked for on `PATH`: give its path)".to_owned(),
            );
        }
        let digest = digest_of(&file);
        if check_access {
            let problems = ownership::program_problems(&file, checked);
            if !problems.is_empty() {
                let texts: Vec<String> = problems.into_iter().map(|problem| problem.text).collect();
                return Self::refused(
                    name.to_owned(),
                    config,
                    digest,
                    format!("refused: {}", texts.join("; ")),
                );
            }
        }
        let current: Arc<Mutex<Arc<Run>>> = Arc::default();
        let start = Start {
            command: config.command.clone(),
            env: config
                .env
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .collect(),
            cwd: config.cwd.clone(),
            grace: config.stop_grace.0,
            first_message: config.start_timeout.0,
            current: Arc::clone(&current),
        };
        let timeout = (!config.call_timeout.0.is_zero()).then_some(config.call_timeout.0);
        let options = LinkOptions::new(Trust::Local)
            .max_calls(config.max_calls)
            .max_frame(config.max_frame)
            .timeout(timeout);
        let source = RemoteSource::new(Box::new(start), options);
        let entry = Self {
            name: name.to_owned(),
            source_name: format!("program:{name}"),
            config: config.clone(),
            file,
            digest,
            source: Some(source.clone()),
            current,
            status: Mutex::new(Status {
                state: ProgramState::Failed,
                detail: String::new(),
                starts: Vec::new(),
                restarts: 0,
                ended: None,
                providers: Vec::new(),
            }),
        };
        if let Err(error) = registry.add_source(&entry.source_name, source) {
            lock(&entry.status).detail = format!("cannot be added: {error}");
            entry.log();
            return entry;
        }
        let before = ids_of(registry);
        let refreshed = registry.refresh();
        let refusals: Vec<String> = refreshed
            .refused
            .iter()
            .filter(|refused| refused.source == entry.source_name)
            .map(|refused| refused.reason.clone())
            .collect();
        let skipped: Vec<String> = refreshed
            .skipped
            .iter()
            .filter(|(source, _, _)| *source == entry.source_name)
            .map(|(_, id, why)| format!("{id} skipped: {why}"))
            .collect();
        entry.settle(registry, &before, &refusals, &skipped);
        entry
    }

    /// Decides, after a refresh, whether the program runs and what it offers, and logs it.
    fn settle(
        &self,
        registry: &Registry,
        before: &[String],
        refusals: &[String],
        skipped: &[String],
    ) {
        let offered: Vec<String> = ids_of(registry)
            .into_iter()
            .filter(|id| !before.contains(id))
            .collect();
        let exit = self.run().poll();
        {
            let mut status = lock(&self.status);
            if exit.is_none() && refusals.is_empty() {
                status.state = ProgramState::Running;
                status.ended = None;
                // A program started again offers what it did before, under ids the registry already knew.
                let offered = if offered.is_empty() {
                    status.providers.clone()
                } else {
                    offered
                };
                let mut detail = format!(
                    "{} provider{}",
                    offered.len(),
                    if offered.len() == 1 { "" } else { "s" }
                );
                if !offered.is_empty() {
                    detail.push_str(": ");
                    detail.push_str(&offered.join(", "));
                }
                for why in skipped {
                    detail.push_str("; ");
                    detail.push_str(why);
                }
                status.detail = detail;
                status.providers = offered;
            } else {
                status.state = ProgramState::Failed;
                status.ended = Some(Instant::now());
                let mut why: Vec<String> = refusals.to_vec();
                if let Some(exit) = exit {
                    why.push(format!("the program exited ({exit})"));
                }
                status.detail = why.join("; ");
            }
        }
        self.log();
    }
}

/// `sha256:` and the digest of the file, or `None` when it cannot be read.
fn digest_of(file: &std::path::Path) -> Option<String> {
    let mut reader = File::open(file).ok()?;
    let (digests, _) = digest_reader(&mut reader, &[Algorithm::Sha256], 64 * 1024).ok()?;
    digests
        .get(&Algorithm::Sha256)
        .map(|hex| format!("sha256:{hex}"))
}
