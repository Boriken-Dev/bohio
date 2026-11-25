//! Helpers the integration tests share: an in-process link, the helper program, process checks.
#![forbid(unsafe_code)]
#![allow(dead_code, reason = "each test binary uses some of these")]

pub mod embedded;
pub mod procs;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bohio::config::{
    CompressionConfig, DeltaConfig, ExecutionConfig, FilesystemConfig, HashConfig, SearchConfig,
    ShellConfig, WalkConfig,
};
use bohio::exec::{self, Context};
use bohio::limit::Slots;
use bohio::policy::Grant;
use bohio::{control, fs, shell};
use bohio_client::relay::Plain;
use bohio_client::{Error, Open};
use bohio_proto::exec::Exit;
use bohio_proto::info::Info;
use bohio_proto::{ErrorBody, protocol};
use coqui_link::mux::LinkStream;
use coqui_transport::frame::LengthPrefix;
use coqui_transport::{Channel, StreamChannel, StreamChannelBuilder, memory};

/// A wait long enough for any exchange on this machine, short enough to fail a hung test.
pub const WAIT: Option<Duration> = Some(Duration::from_secs(20));

/// What a served stream returned.
pub type Served = Result<Exit, ErrorBody>;

/// What a served filesystem or control stream returned.
pub type ServedFs = Result<(), ErrorBody>;

/// What a served shell session returned.
pub type ServedShell = Result<bohio_proto::shell::Exit, ErrorBody>;

/// A client link to a host engine in the same process: each opened stream is a pair of memory
/// channels, with the engine served on a thread of its own.
pub struct Link {
    pub grant: Arc<Grant>,
    pub config: Arc<ExecutionConfig>,
    pub slots: Arc<Slots>,
    pub files: Arc<FilesystemConfig>,
    pub hash: Arc<HashConfig>,
    /// The `[delta]` settings of the host; a test changes them with [`Link::deltaing`].
    pub delta: Arc<DeltaConfig>,
    /// The `[compression]` settings of the host; a test changes them with [`Link::compressing`].
    pub compression: Arc<CompressionConfig>,
    /// The peer id the host sees; a prepared read is bound to it.
    pub peer: String,
    pub transfers: Arc<Slots>,
    pub follows: Arc<Slots>,
    pub hash_slots: Arc<Slots>,
    /// The `[walk]` settings of the host; a test changes them with [`Link::walking`].
    pub walk: Arc<WalkConfig>,
    pub walk_slots: Arc<Slots>,
    /// The `[search]` settings of the host; a test changes them with [`Link::searching`].
    pub search: Arc<SearchConfig>,
    pub search_slots: Arc<Slots>,
    /// What the peer is granted now, when it differs from `grant`: a hash stream reads it before each item, as the
    /// node does, so a test narrows a grant while a stream is open.
    pub live: Arc<Mutex<Option<Grant>>>,
    pub info: Arc<Info>,
    pub shells: Arc<ShellConfig>,
    pub sessions: Arc<Slots>,
    /// The bytes each direction of a stream holds before its writer waits.
    pub capacity: usize,
    /// The host's own `PATH`, where listed programs are found. Starts as this process's.
    pub path: Arc<std::ffi::OsString>,
    served: Mutex<Vec<JoinHandle<Served>>>,
    served_fs: Mutex<Vec<JoinHandle<ServedFs>>>,
    served_shell: Mutex<Vec<JoinHandle<ServedShell>>>,
}

impl Link {
    pub fn new(grant: Grant, config: ExecutionConfig) -> Self {
        Self::build(
            grant,
            config,
            FilesystemConfig::default(),
            ShellConfig::default(),
        )
    }

    /// A link to the shell engine of a host that grants `grant`.
    pub fn shells(grant: Grant, shells: ShellConfig) -> Self {
        Self::build(
            grant,
            ExecutionConfig::default(),
            FilesystemConfig::default(),
            shells,
        )
    }

    /// A link to the filesystem engines of a host that grants `grant`.
    pub fn files(grant: Grant, files: FilesystemConfig) -> Self {
        Self::build(
            grant,
            ExecutionConfig::default(),
            files,
            ShellConfig::default(),
        )
    }

    /// As [`Link::files`], with the `[hash]` settings `hash`.
    pub fn hashing(grant: Grant, files: FilesystemConfig, hash: HashConfig) -> Self {
        let mut link = Self::files(grant, files);
        link.hash_slots = Arc::new(Slots::new(hash.max_streams));
        link.hash = Arc::new(hash);
        link
    }

    /// This host with the `[delta]` settings `delta`.
    #[must_use]
    pub fn deltaing(mut self, delta: DeltaConfig) -> Self {
        self.delta = Arc::new(delta);
        self
    }

    /// This host with the `[walk]` settings `walk`.
    #[must_use]
    pub fn walking(mut self, walk: WalkConfig) -> Self {
        self.walk_slots = Arc::new(Slots::new(walk.max_streams));
        self.walk = Arc::new(walk);
        self
    }

    /// This host with the `[search]` settings `search`.
    #[must_use]
    pub fn searching(mut self, search: SearchConfig) -> Self {
        self.search_slots = Arc::new(Slots::new(search.max_searches));
        self.search = Arc::new(search);
        self
    }

    /// This host with the `[compression]` settings `compression`.
    #[must_use]
    pub fn compressing(mut self, compression: CompressionConfig) -> Self {
        self.compression = Arc::new(compression);
        self
    }

    /// From now on the peer is granted `grant`: a hash stream's next item is decided by it.
    pub fn narrow(&self, grant: Grant) {
        *self.live.lock().unwrap() = Some(grant);
    }

    fn build(
        grant: Grant,
        config: ExecutionConfig,
        files: FilesystemConfig,
        shells: ShellConfig,
    ) -> Self {
        let slots = Slots::new(config.max_processes);
        let transfers = Slots::new(files.max_transfers);
        let follows = Slots::new(files.max_follows);
        let sessions = Slots::new(shells.max_sessions);
        Self {
            grant: Arc::new(grant),
            config: Arc::new(config),
            slots: Arc::new(slots),
            hash_slots: Arc::new(Slots::new(HashConfig::default().max_streams)),
            hash: Arc::new(HashConfig::default()),
            walk: Arc::new(WalkConfig::default()),
            walk_slots: Arc::new(Slots::new(WalkConfig::default().max_streams)),
            search: Arc::new(SearchConfig::default()),
            search_slots: Arc::new(Slots::new(SearchConfig::default().max_searches)),
            delta: Arc::new(DeltaConfig::default()),
            compression: Arc::new(CompressionConfig::default()),
            peer: "test-peer".to_owned(),
            live: Arc::new(Mutex::new(None)),
            files: Arc::new(files),
            transfers: Arc::new(transfers),
            follows: Arc::new(follows),
            info: Arc::new(Info {
                protocol: 1,
                ..Info::default()
            }),
            shells: Arc::new(shells),
            sessions: Arc::new(sessions),
            capacity: 64 * 1024,
            path: Arc::new(std::env::var_os("PATH").unwrap_or_default()),
            served: Mutex::new(Vec::new()),
            served_fs: Mutex::new(Vec::new()),
            served_shell: Mutex::new(Vec::new()),
        }
    }

    /// Waits for every shell session served so far to finish (it fails, not hangs, when one does
    /// not) and returns what each returned.
    pub fn finished_shell(&self) -> Vec<ServedShell> {
        let handles = std::mem::take(&mut *self.served_shell.lock().unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        handles
            .into_iter()
            .map(|handle| {
                while !handle.is_finished() {
                    assert!(
                        Instant::now() < deadline,
                        "a shell session did not finish: it hangs instead of ending"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                handle.join().expect("the engine thread did not panic")
            })
            .collect()
    }

    /// Waits for every filesystem and control stream served so far to finish.
    pub fn finished_fs(&self) -> Vec<ServedFs> {
        let handles = std::mem::take(&mut *self.served_fs.lock().unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        handles
            .into_iter()
            .map(|handle| {
                while !handle.is_finished() {
                    assert!(
                        Instant::now() < deadline,
                        "an engine thread did not finish: it hangs instead of ending"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                handle.join().expect("the engine thread did not panic")
            })
            .collect()
    }

    /// Waits for every stream served so far to finish and returns what each returned.
    pub fn finished(&self) -> Vec<Served> {
        let handles = std::mem::take(&mut *self.served.lock().unwrap());
        handles
            .into_iter()
            .map(|handle| handle.join().expect("the engine thread did not panic"))
            .collect()
    }
}

fn channel(stream: memory::Stream) -> StreamChannel {
    StreamChannelBuilder::new()
        .codec(LengthPrefix::u32_be())
        .stream(stream)
        // A channel that reads ahead or queues without bound would hide a transfer's own memory.
        .pending(4)
        .queued_bytes(1024 * 1024)
        // Closing a channel whose peer reads nothing waits this long; a test that abandons one must not wait 30 s.
        .send_limit(Duration::from_secs(8))
        .linger(Duration::from_millis(50))
        .build()
        .expect("the channel starts")
}

impl Open for Link {
    fn open(&self, protocol_name: &str, metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        let (host_end, client_end) = memory::pipe(self.capacity);
        let host: Arc<dyn Channel> = Arc::new(channel(host_end));
        let metadata = metadata.to_vec();
        if protocol_name == protocol::SHELL {
            let (grant, shells, sessions, path) = (
                Arc::clone(&self.grant),
                Arc::clone(&self.shells),
                Arc::clone(&self.sessions),
                Arc::clone(&self.path),
            );
            let handle = std::thread::spawn(move || {
                let context = shell::Context {
                    grant: &grant,
                    config: &shells,
                    slots: &sessions,
                    path: &path,
                    sources: &bohio::access::Sources::default(),
                };
                let result = shell::serve(Arc::clone(&host), &metadata, &context);
                host.close();
                result
            });
            self.served_shell.lock().unwrap().push(handle);
            return Ok(Box::new(Plain(channel(client_end))));
        }
        if protocol_name != protocol::EXEC {
            let name = protocol_name.to_owned();
            let (grant, files, transfers, info) = (
                Arc::clone(&self.grant),
                Arc::clone(&self.files),
                Arc::clone(&self.transfers),
                Arc::clone(&self.info),
            );
            let follows = Arc::clone(&self.follows);
            let path = Arc::clone(&self.path);
            let peer = self.peer.clone();
            let (hash, hash_slots, live, compression, delta) = (
                Arc::clone(&self.hash),
                Arc::clone(&self.hash_slots),
                Arc::clone(&self.live),
                Arc::clone(&self.compression),
                Arc::clone(&self.delta),
            );
            let (walk, walk_slots) = (Arc::clone(&self.walk), Arc::clone(&self.walk_slots));
            let (search, search_slots) = (Arc::clone(&self.search), Arc::clone(&self.search_slots));
            let handle = std::thread::spawn(move || {
                let now = || {
                    Ok(live
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| (*grant).clone()))
                };
                let sources = bohio::access::Sources::default();
                let files_source = sources.files(None).expect("native files").arc();
                let context = fs::Context {
                    source: fs::FilesRef::of(&files_source),
                    grant: &grant,
                    config: &files,
                    hash: &hash,
                    delta: &delta,
                    compression: &compression,
                    peer: &peer,
                    slots: &transfers,
                    follows: &follows,
                    current: Some(control::Current(&now)),
                };
                let result = match name.as_str() {
                    protocol::CONTROL => {
                        let context = control::Context {
                            gui: None,
                            access: None,
                            files: None,
                            filesystem: context,
                            info: &info,
                            current: None,
                            sessions: None,
                            path: Some(path.as_os_str()),
                        };
                        control::serve(&host, &context);
                        Ok(())
                    }
                    protocol::READ => {
                        fs::serve_read(Arc::clone(&host), &metadata, &context).map(|_| ())
                    }
                    protocol::WRITE => {
                        fs::serve_write(Arc::clone(&host), &metadata, &context).map(|_| ())
                    }
                    protocol::WALK => {
                        let context = fs::WalkContext {
                            files: context,
                            walk: &walk,
                            slots: &walk_slots,
                        };
                        fs::serve_walk(Arc::clone(&host), &metadata, &context).map(|_| ())
                    }
                    protocol::SEARCH => {
                        let context = fs::SearchContext {
                            files: context,
                            search: &search,
                            slots: &search_slots,
                        };
                        fs::serve_search(Arc::clone(&host), &metadata, &context).map(|_| ())
                    }
                    protocol::HASH => {
                        let context = fs::HashContext {
                            files: context,
                            slots: &hash_slots,
                            current: Some(control::Current(&now)),
                        };
                        fs::serve_hash(Arc::clone(&host), &metadata, &context).map(|_| ())
                    }
                    other => panic!("the test link does not serve {other}"),
                };
                host.close();
                result
            });
            self.served_fs.lock().unwrap().push(handle);
            return Ok(Box::new(Plain(channel(client_end))));
        }
        let (grant, config, slots, path, compression) = (
            Arc::clone(&self.grant),
            Arc::clone(&self.config),
            Arc::clone(&self.slots),
            Arc::clone(&self.path),
            Arc::clone(&self.compression),
        );
        let handle = std::thread::spawn(move || {
            let sources = bohio::access::Sources::default();
            let context = Context {
                grant: &grant,
                config: &config,
                compression: &compression,
                slots: &slots,
                path: &path,
                sources: &sources,
            };
            let result = exec::serve(Arc::clone(&host), &metadata, &context);
            host.close();
            result
        });
        self.served.lock().unwrap().push(handle);
        Ok(Box::new(Plain(channel(client_end))))
    }
}

/// The helper program built from `examples/exec_helper.rs`: the `examples` directory of the nearest
/// ancestor of the test executable that has it (cargo keeps intermediate files under `deps` or under
/// `build/<package>/<hash>`, and the built example beside them).
pub fn helper() -> String {
    let exe = std::env::current_exe().expect("the test's own path");
    let name = format!("exec_helper{}", std::env::consts::EXE_SUFFIX);
    let found = exe
        .ancestors()
        .skip(1)
        .map(|directory| directory.join("examples").join(&name))
        .find(|candidate| candidate.is_file());
    let path: PathBuf = found.unwrap_or_else(|| {
        panic!(
            "the helper is not built: run `cargo build -p bohio --example exec_helper` before a narrow cargo test (looked above {})",
            exe.display()
        )
    });
    path.to_string_lossy().into_owned()
}

/// Whether a process with this id exists and is not a zombie.
pub fn alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, System};
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some_and(|process| {
        !matches!(
            process.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        )
    })
}

/// Waits until `pid` is gone, up to `within`.
pub fn gone_within(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !alive(pid)
}

/// The ids of the processes running `program` (by its file name) with `argument` among their
/// arguments. A zombie is not listed.
pub fn processes_of(program: &str, argument: &str) -> Vec<u32> {
    use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    let name = std::path::Path::new(program).file_name();
    system
        .processes()
        .values()
        .filter(|process| {
            // Linux lists a process's threads beside it, with the same command line.
            process.thread_kind().is_none()
                && !matches!(
                    process.status(),
                    ProcessStatus::Zombie | ProcessStatus::Dead
                )
                && process
                    .cmd()
                    .first()
                    .is_some_and(|first| std::path::Path::new(first).file_name() == name)
                && process.cmd().iter().skip(1).any(|part| part == argument)
        })
        .map(|process| process.pid().as_u32())
        .collect()
}

/// Ends the processes `ids`, so a test that found survivors leaves none.
pub fn end_processes(ids: &[u32]) {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    for id in ids {
        if let Some(process) = system.process(Pid::from_u32(*id)) {
            process.kill();
        }
    }
}
