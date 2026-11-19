//! Helpers the integration tests share: an in-process link, the helper program, process checks.
#![allow(dead_code, reason = "each test binary uses some of these")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bohio::config::{ExecutionConfig, FilesystemConfig};
use bohio::exec::{self, Context};
use bohio::limit::Slots;
use bohio::policy::Grant;
use bohio::{control, fs};
use bohio_client::{Error, Open};
use bohio_proto::exec::Exit;
use bohio_proto::info::Info;
use bohio_proto::{ErrorBody, protocol};
use coqui_transport::frame::LengthPrefix;
use coqui_transport::{Channel, StreamChannel, StreamChannelBuilder, memory};

/// A wait long enough for any exchange on this machine, short enough to fail a hung test.
pub const WAIT: Option<Duration> = Some(Duration::from_secs(20));

/// What a served stream returned.
pub type Served = Result<Exit, ErrorBody>;

/// What a served filesystem or control stream returned.
pub type ServedFs = Result<(), ErrorBody>;

/// A client link to a host engine in the same process: each opened stream is a pair of memory
/// channels, with the engine served on a thread of its own.
pub struct Link {
    pub grant: Arc<Grant>,
    pub config: Arc<ExecutionConfig>,
    pub slots: Arc<Slots>,
    pub files: Arc<FilesystemConfig>,
    pub transfers: Arc<Slots>,
    pub info: Arc<Info>,
    /// The bytes each direction of a stream holds before its writer waits.
    pub capacity: usize,
    served: Mutex<Vec<JoinHandle<Served>>>,
    served_fs: Mutex<Vec<JoinHandle<ServedFs>>>,
}

impl Link {
    pub fn new(grant: Grant, config: ExecutionConfig) -> Self {
        Self::build(grant, config, FilesystemConfig::default())
    }

    /// A link to the filesystem engines of a host that grants `grant`.
    pub fn files(grant: Grant, files: FilesystemConfig) -> Self {
        Self::build(grant, ExecutionConfig::default(), files)
    }

    fn build(grant: Grant, config: ExecutionConfig, files: FilesystemConfig) -> Self {
        let slots = Slots::new(config.max_processes);
        let transfers = Slots::new(files.max_transfers);
        Self {
            grant: Arc::new(grant),
            config: Arc::new(config),
            slots: Arc::new(slots),
            files: Arc::new(files),
            transfers: Arc::new(transfers),
            info: Arc::new(Info {
                protocol: 1,
                ..Info::default()
            }),
            capacity: 64 * 1024,
            served: Mutex::new(Vec::new()),
            served_fs: Mutex::new(Vec::new()),
        }
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
    fn open(&self, protocol_name: &str, metadata: &[u8]) -> Result<Box<dyn Channel>, Error> {
        let (host_end, client_end) = memory::pipe(self.capacity);
        let host: Arc<dyn Channel> = Arc::new(channel(host_end));
        let metadata = metadata.to_vec();
        if protocol_name != protocol::EXEC {
            let name = protocol_name.to_owned();
            let (grant, files, transfers, info) = (
                Arc::clone(&self.grant),
                Arc::clone(&self.files),
                Arc::clone(&self.transfers),
                Arc::clone(&self.info),
            );
            let handle = std::thread::spawn(move || {
                let context = fs::Context {
                    grant: &grant,
                    config: &files,
                    slots: &transfers,
                };
                let result = match name.as_str() {
                    protocol::CONTROL => {
                        let context = control::Context {
                            filesystem: context,
                            info: &info,
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
                    other => panic!("the test link does not serve {other}"),
                };
                host.close();
                result
            });
            self.served_fs.lock().unwrap().push(handle);
            return Ok(Box::new(channel(client_end)));
        }
        let (grant, config, slots) = (
            Arc::clone(&self.grant),
            Arc::clone(&self.config),
            Arc::clone(&self.slots),
        );
        let handle = std::thread::spawn(move || {
            let context = Context {
                grant: &grant,
                config: &config,
                slots: &slots,
            };
            let result = exec::serve(Arc::clone(&host), &metadata, &context);
            host.close();
            result
        });
        self.served.lock().unwrap().push(handle);
        Ok(Box::new(channel(client_end)))
    }
}

/// The helper program built from `examples/exec_helper.rs`.
pub fn helper() -> String {
    let exe = std::env::current_exe().expect("the test's own path");
    let name = format!("exec_helper{}", std::env::consts::EXE_SUFFIX);
    let path: PathBuf = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("target directory")
        .join("examples")
        .join(name);
    assert!(
        path.is_file(),
        "the helper is not built: run `cargo build -p bohio --example exec_helper` before a narrow cargo test ({})",
        path.display()
    );
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
