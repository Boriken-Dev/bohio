//! The scenarios: what is timed, how a sample is taken and what each leaves in the result.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

use bohio_client::connect::Connection;
use bohio_client::dial::Endpoint;
use bohio_client::link::Settings;
#[cfg(feature = "hash")]
use bohio_proto::hash::Algorithm;
use coqui_link::auth::Proof;
use sha2::{Digest, Sha256};

use crate::corpus::{Xorshift, parse_size};
use crate::host::Cpu;
use crate::report::{Metric, ms};
use crate::{compat, raw};

/// How many small files the corpus holds when it is made with its default count: `hash_many_N` cycles over them.
#[cfg(feature = "hash")]
const SMALL_FILES: u64 = 100;

/// What a scenario is, parsed from its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Dial and proof to a ready link.
    Connect,
    /// A control stream and one `info` on a ready link.
    Info,
    /// `n` serial `stat`s on one control stream.
    StatSerial(u64),
    /// `n` control streams opened and closed.
    OpenClose(u64),
    /// A read of the random corpus file of this size, on a new link per sample or on one link.
    Read { size: String, warm: bool },
    /// A replace of this many bytes.
    Write { size: String },
    /// One `echo` through exec.
    ExecEcho,
    /// A program that writes this many bytes to standard output.
    ExecOut { size: String },
    /// The host's SHA-256 of a file.
    Checksum { size: String },
    /// The host's digest of a file by one algorithm.
    #[cfg(feature = "hash")]
    Hash { algorithm: Algorithm, size: String },
    /// The host's SHA-256 of this many small files on one stream.
    #[cfg(feature = "hash")]
    HashMany(u64),
    /// One algorithm over memory, no I/O: the CPU control of `Hash`.
    #[cfg(feature = "hash")]
    CpuHash { algorithm: Algorithm, size: String },
    /// `bohio remote ... read` as a child process.
    RemoteRead { size: String },
    /// `bohio remote ... info` as a child process.
    RemoteInfo,
    /// A raw TCP download.
    RawDown { size: String },
    /// A raw TCP download or upload on a connection that has already carried one (`up` is the upload).
    RawWarm { up: bool, size: String },
    /// `n` raw TCP downloads at the same time: what the path carries across flows.
    RawParDown { n: u64, size: String },
    /// A raw TCP upload.
    RawUp { size: String },
    /// A raw TCP connect.
    RawConnect,
    /// SHA-256 over memory, no I/O: the CPU control.
    CpuSha256 { size: String },
    /// The parts of one cold read: dial, proof, open, first byte, body.
    TimelineRead { size: String },
    /// The parts of one cold `info`: dial, proof, control open, the call.
    TimelineInfo,
}

impl Kind {
    /// Parses a scenario name.
    ///
    /// # Errors
    ///
    /// A name no scenario has, or one whose size does not parse.
    pub fn parse(name: &str) -> Result<Self, String> {
        let parts: Vec<&str> = name.split('_').collect();
        let count = |text: &str| -> Result<u64, String> {
            text.strip_prefix('x')
                .and_then(|digits| digits.parse().ok())
                .ok_or_else(|| format!("`{text}` in `{name}` is not a count like x100"))
        };
        let size = |text: &str| -> Result<String, String> {
            parse_size(text)?;
            Ok(text.to_owned())
        };
        // The host's SHA-256 of a file keeps the metric name the committed baselines use.
        if let Some(rest) = name.strip_prefix("checksum_") {
            return Ok(Self::Checksum { size: size(rest)? });
        }
        Ok(match parts.as_slice() {
            ["connect"] => Self::Connect,
            ["info"] => Self::Info,
            ["stat", n, "serial"] => Self::StatSerial(count(n)?),
            ["open", "close", n] => Self::OpenClose(count(n)?),
            ["read", s, mode @ ("cold" | "warm")] => Self::Read {
                size: size(s)?,
                warm: *mode == "warm",
            },
            ["write", s] => Self::Write { size: size(s)? },
            ["exec", "echo"] => Self::ExecEcho,
            ["exec", "out", s] => Self::ExecOut { size: size(s)? },
            #[cfg(feature = "hash")]
            ["hash", "many", n] => Self::HashMany(
                n.parse()
                    .map_err(|_| format!("`{n}` in `{name}` is not a count"))?,
            ),
            #[cfg(feature = "hash")]
            ["hash", algorithm, s] => Self::Hash {
                algorithm: known(algorithm, name)?,
                size: size(s)?,
            },
            #[cfg(feature = "hash")]
            ["cpu", "hash", algorithm, s] => Self::CpuHash {
                algorithm: known(algorithm, name)?,
                size: size(s)?,
            },
            ["remote", "read", s] => Self::RemoteRead { size: size(s)? },
            ["remote", "info"] => Self::RemoteInfo,
            ["raw", "down", s] => Self::RawDown { size: size(s)? },
            ["raw", "up", s] => Self::RawUp { size: size(s)? },
            ["raw", "warm", dir @ ("down" | "up"), s] => Self::RawWarm {
                up: *dir == "up",
                size: size(s)?,
            },
            ["raw", "par", n, "down", s] => Self::RawParDown {
                n: count(n)?,
                size: size(s)?,
            },
            ["raw", "connect"] => Self::RawConnect,
            ["cpu", "sha256", s] => Self::CpuSha256 { size: size(s)? },
            ["timeline", "read", s] => Self::TimelineRead { size: size(s)? },
            ["timeline", "info"] => Self::TimelineInfo,
            _ => return Err(format!("no scenario is named `{name}`")),
        })
    }
}

/// The algorithm a scenario name spells.
#[cfg(feature = "hash")]
fn known(text: &str, name: &str) -> Result<Algorithm, String> {
    let algorithm = Algorithm::from_name(text);
    if algorithm.is_known() {
        Ok(algorithm)
    } else {
        Err(format!("`{text}` in `{name}` is not a hash algorithm"))
    }
}

/// Everything a scenario needs to reach the host.
pub struct Env {
    /// The host's endpoint as the client library dials it.
    pub endpoint: Endpoint,
    /// The same endpoint as text, for the `bohio remote` child.
    pub endpoint_text: String,
    /// The client's proof.
    pub proof: Proof,
    /// The client's link bounds (`--set` applied).
    pub settings: Settings,
    /// The corpus directory on the host, in its path text.
    pub root: String,
    /// The program exec runs, in the host's path text; none skips the exec scenarios.
    pub program: Option<String>,
    /// The `bohio` the remote scenarios run.
    pub bohio: PathBuf,
    /// The client's configuration file.
    pub client_config: PathBuf,
    /// The host's id to trust instead of the stored keys.
    pub server: Option<String>,
    /// The raw control server.
    pub raw: Option<SocketAddr>,
    /// The host process tree's CPU time, when the host runs where the harness can see it.
    pub cpu: Option<Cpu>,
    /// Samples per metric.
    pub samples: usize,
    /// The label that ends every metric name: `<transport>_<link>`.
    pub suffix: String,
}

/// What the scenarios produced.
#[derive(Default)]
pub struct Output {
    /// Metrics by name.
    pub metrics: BTreeMap<String, Metric>,
    /// Timeline curves by name.
    pub curves: BTreeMap<String, Vec<Vec<[u64; 2]>>>,
}

/// Runs scenarios against one host, keeping one link for the warm ones.
pub struct Runner {
    env: Env,
    warm: Option<Connection>,
    /// What has been measured.
    pub out: Output,
}

/// Wall milliseconds and host CPU milliseconds of each sample.
struct Samples {
    wall: Vec<f64>,
    cpu: Vec<f64>,
}

fn timed(
    cpu: &mut Option<Cpu>,
    samples: usize,
    mut one: impl FnMut() -> Result<(), String>,
) -> Result<Samples, String> {
    let mut taken = Samples {
        wall: Vec::new(),
        cpu: Vec::new(),
    };
    for _ in 0..samples {
        let before = cpu.as_mut().map(Cpu::used_ms);
        let start = Instant::now();
        one()?;
        taken.wall.push(ms(start.elapsed()));
        if let (Some(cpu), Some(before)) = (cpu.as_mut(), before) {
            taken.cpu.push(cpu.used_ms().saturating_sub(before) as f64);
        }
    }
    Ok(taken)
}

fn warm_link<'a>(env: &Env, warm: &'a mut Option<Connection>) -> Result<&'a Connection, String> {
    if !warm.as_ref().is_some_and(Connection::is_up) {
        *warm = Some(compat::connect(&env.endpoint, &env.proof, &env.settings)?);
    }
    warm.as_ref().ok_or_else(|| "no link".to_owned())
}

impl Runner {
    /// A runner for `env`.
    pub const fn new(env: Env) -> Self {
        Self {
            env,
            warm: None,
            out: Output {
                metrics: BTreeMap::new(),
                curves: BTreeMap::new(),
            },
        }
    }

    fn record(&mut self, name: &str, samples: &Samples, bytes: Option<u64>) {
        let suffix = self.env.suffix.clone();
        if let Some(metric) = Metric::of(&samples.wall, bytes) {
            self.out.metrics.insert(format!("{name}_{suffix}"), metric);
        }
        if let Some(metric) = Metric::of(&samples.cpu, None) {
            self.out
                .metrics
                .insert(format!("{name}_host_cpu_{suffix}"), metric);
        }
    }

    fn record_values(&mut self, name: &str, values: &[f64], bytes: Option<u64>) {
        let suffix = self.env.suffix.clone();
        if let Some(metric) = Metric::of(values, bytes) {
            self.out.metrics.insert(format!("{name}_{suffix}"), metric);
        }
    }

    fn path(&self, file: &str) -> String {
        format!("{}/{file}", self.env.root.trim_end_matches(['/', '\\']))
    }

    /// Runs the scenario called `name`.
    ///
    /// # Errors
    ///
    /// An unknown name, a scenario whose setup is missing, or the first failed sample.
    #[expect(clippy::too_many_lines, reason = "one arm per scenario")]
    pub fn run(&mut self, name: &str) -> Result<(), String> {
        let kind = Kind::parse(name)?;
        let samples = self.env.samples;
        match kind.clone() {
            Kind::Connect => {
                let Env {
                    endpoint,
                    proof,
                    settings,
                    cpu,
                    ..
                } = &mut self.env;
                let taken = timed(cpu, samples, || {
                    compat::connect(endpoint, proof, settings).map(drop)
                })?;
                self.record(name, &taken, None);
            }
            Kind::Info => {
                let link = warm_link(&self.env, &mut self.warm)?;
                let taken = timed(&mut self.env.cpu, samples, || compat::info(link))?;
                self.record(name, &taken, None);
            }
            Kind::StatSerial(count) => {
                let link = warm_link(&self.env, &mut self.warm)?;
                let fs = compat::fs(link)?;
                let file = self.path("random-1KiB");
                let taken = timed(&mut self.env.cpu, samples, || {
                    for _ in 0..count {
                        compat::stat(&fs, &file)?;
                    }
                    Ok(())
                })?;
                self.record(name, &taken, None);
            }
            Kind::OpenClose(count) => {
                let link = warm_link(&self.env, &mut self.warm)?;
                let taken = timed(&mut self.env.cpu, samples, || {
                    for _ in 0..count {
                        compat::open_close(link)?;
                    }
                    Ok(())
                })?;
                self.record(name, &taken, None);
            }
            Kind::Read { size, warm: true } => {
                let file = self.path(&format!("random-{size}"));
                let link = warm_link(&self.env, &mut self.warm)?;
                compat::read(link, &file)?;
                let taken = timed(&mut self.env.cpu, samples, || {
                    compat::read(link, &file).map(drop)
                })?;
                self.record(name, &taken, Some(parse_size(&size)?));
            }
            Kind::Read { size, warm: false } => {
                let file = self.path(&format!("random-{size}"));
                let Env {
                    endpoint,
                    proof,
                    settings,
                    cpu,
                    ..
                } = &mut self.env;
                let taken = timed(cpu, samples, || {
                    let link = compat::connect(endpoint, proof, settings)?;
                    compat::read(&link, &file).map(drop)
                })?;
                self.record(name, &taken, Some(parse_size(&size)?));
            }
            Kind::Write { size } => {
                let bytes = parse_size(&size)?;
                let mut block = vec![0_u8; 1 << 20];
                Xorshift::new(7).fill(&mut block);
                let target = self.path(&format!("written/write-{size}.bin"));
                let link = warm_link(&self.env, &mut self.warm)?;
                let fs = compat::fs(link)?;
                let taken = timed(&mut self.env.cpu, samples, || {
                    compat::write(link, &target, bytes, &block)
                })?;
                compat::remove(&fs, &target);
                self.record(name, &taken, Some(bytes));
            }
            Kind::ExecEcho | Kind::ExecOut { .. } => {
                let program = self
                    .env
                    .program
                    .clone()
                    .ok_or("no --program: the exec scenarios are skipped")?;
                let (argv, bytes) = match &kind {
                    Kind::ExecOut { size } => {
                        let bytes = parse_size(size)?;
                        (
                            vec![program, "flood".to_owned(), bytes.to_string()],
                            Some(bytes),
                        )
                    }
                    _ => (vec![program, "echo".to_owned(), "bench".to_owned()], None),
                };
                let link = warm_link(&self.env, &mut self.warm)?;
                let taken = timed(&mut self.env.cpu, samples, || {
                    compat::exec(link, &argv).map(drop)
                })?;
                self.record(name, &taken, bytes);
            }
            Kind::Checksum { size } => {
                let file = self.path(&format!("random-{size}"));
                let link = warm_link(&self.env, &mut self.warm)?;
                let taken = timed(&mut self.env.cpu, samples, || compat::checksum(link, &file))?;
                self.record(name, &taken, Some(parse_size(&size)?));
            }
            #[cfg(feature = "hash")]
            Kind::Hash { algorithm, size } => {
                let file = self.path(&format!("random-{size}"));
                let link = warm_link(&self.env, &mut self.warm)?;
                let taken = timed(&mut self.env.cpu, samples, || {
                    compat::hash(link, &file, &algorithm)
                })?;
                self.record(name, &taken, Some(parse_size(&size)?));
            }
            #[cfg(feature = "hash")]
            Kind::HashMany(count) => {
                let files: Vec<String> = (0..count)
                    .map(|index| self.path(&format!("small/{:05}.bin", index % SMALL_FILES)))
                    .collect();
                let link = warm_link(&self.env, &mut self.warm)?;
                let taken = timed(&mut self.env.cpu, samples, || {
                    compat::hash_batch(link, &files)
                })?;
                self.record(name, &taken, None);
            }
            #[cfg(feature = "hash")]
            Kind::CpuHash { algorithm, size } => {
                let bytes = parse_size(&size)?;
                let mut block = vec![0_u8; 1 << 20];
                Xorshift::new(13).fill(&mut block);
                let taken = timed(&mut None, samples, || {
                    let mut hasher =
                        bohio_transfer::hash::Hasher::new(std::slice::from_ref(&algorithm))
                            .map_err(|error| error.to_string())?;
                    let mut left = bytes;
                    while left > 0 {
                        let take = usize::try_from(left.min(block.len() as u64)).unwrap_or(1);
                        hasher.update(&block[..take]);
                        left -= take as u64;
                    }
                    std::hint::black_box(hasher.finish());
                    Ok(())
                })?;
                self.record(name, &taken, Some(bytes));
            }
            Kind::RemoteRead { size } => {
                let file = self.path(&format!("random-{size}"));
                let mut cpu = self.env.cpu.take();
                let taken = timed(&mut cpu, samples, || {
                    remote_child(&self.env, &["read", &file])
                });
                self.env.cpu = cpu;
                self.record(name, &taken?, Some(parse_size(&size)?));
            }
            Kind::RemoteInfo => {
                let mut cpu = self.env.cpu.take();
                let taken = timed(&mut cpu, samples, || remote_child(&self.env, &["info"]));
                self.env.cpu = cpu;
                self.record(name, &taken?, None);
            }
            Kind::RawConnect => {
                let address = self.env.raw.ok_or("no raw control address")?;
                let taken = timed(&mut self.env.cpu, samples, || {
                    raw::connect(address, compat::LIMIT)
                        .map(drop)
                        .map_err(|error| error.to_string())
                })?;
                self.record(name, &taken, None);
            }
            Kind::RawDown { size } | Kind::RawUp { size } => {
                let address = self.env.raw.ok_or("no raw control address")?;
                let bytes = parse_size(&size)?;
                let down = matches!(kind, Kind::RawDown { .. });
                let taken = timed(&mut self.env.cpu, samples, || {
                    let done = if down {
                        raw::download(address, bytes, compat::LIMIT)
                    } else {
                        raw::upload(address, bytes, compat::LIMIT)
                    };
                    done.map_err(|error| error.to_string())
                })?;
                self.record(name, &taken, Some(bytes));
            }
            Kind::RawWarm { up, size } => {
                let address = self.env.raw.ok_or("no raw control address")?;
                let bytes = parse_size(&size)?;
                let mut link =
                    raw::Link::open(address, compat::LIMIT).map_err(|error| error.to_string())?;
                let one = |link: &mut raw::Link| {
                    if up {
                        link.upload(bytes)
                    } else {
                        link.download(bytes)
                    }
                    .map_err(|error| error.to_string())
                };
                one(&mut link)?;
                let taken = timed(&mut self.env.cpu, samples, || one(&mut link))?;
                self.record(name, &taken, Some(bytes));
            }
            Kind::RawParDown { n, size } => {
                let address = self.env.raw.ok_or("no raw control address")?;
                let bytes = parse_size(&size)?;
                let taken = timed(&mut self.env.cpu, samples, || {
                    std::thread::scope(|scope| {
                        let flows: Vec<_> = (0..n)
                            .map(|_| scope.spawn(|| raw::download(address, bytes, compat::LIMIT)))
                            .collect();
                        for flow in flows {
                            flow.join()
                                .map_err(|_| "a download thread failed".to_owned())?
                                .map_err(|error| error.to_string())?;
                        }
                        Ok(())
                    })
                })?;
                self.record(name, &taken, Some(bytes * n));
            }
            Kind::CpuSha256 { size } => {
                let bytes = parse_size(&size)?;
                let mut block = vec![0_u8; 1 << 20];
                Xorshift::new(11).fill(&mut block);
                let taken = timed(&mut None, samples, || {
                    let mut hash = Sha256::new();
                    let mut left = bytes;
                    while left > 0 {
                        let take = usize::try_from(left.min(block.len() as u64)).unwrap_or(1);
                        hash.update(&block[..take]);
                        left -= take as u64;
                    }
                    std::hint::black_box(hash.finalize());
                    Ok(())
                })?;
                self.record(name, &taken, Some(bytes));
            }
            Kind::TimelineRead { size } => self.timeline_read(name, &size)?,
            Kind::TimelineInfo => self.timeline_info(name)?,
        }
        // Each scenario gets its own link: the host limits how fast streams may be opened per link, and
        // one scenario's opens must not count against the next.
        self.warm = None;
        Ok(())
    }

    fn timeline_info(&mut self, name: &str) -> Result<(), String> {
        let mut parts: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
        for _ in 0..self.env.samples {
            let start = Instant::now();
            let channel = compat::dial(&self.env.endpoint, &self.env.settings)?;
            let dialed = start.elapsed();
            let link = compat::prove(channel, &self.env.proof, &self.env.settings)?;
            let proven = start.elapsed();
            let control =
                bohio_client::control::Control::open(&link).map_err(|error| error.to_string())?;
            let opened = start.elapsed();
            control
                .info(Some(compat::LIMIT))
                .map_err(|error| error.to_string())?;
            let called = start.elapsed();
            parts.entry("dial").or_default().push(ms(dialed));
            parts.entry("proof").or_default().push(ms(proven - dialed));
            parts.entry("open").or_default().push(ms(opened - proven));
            parts.entry("call").or_default().push(ms(called - opened));
            parts.entry("total").or_default().push(ms(called));
        }
        for (part, values) in parts {
            self.record_values(&format!("{name}_{part}"), &values, None);
        }
        Ok(())
    }

    fn timeline_read(&mut self, name: &str, size: &str) -> Result<(), String> {
        let file = self.path(&format!("random-{size}"));
        let bytes = parse_size(size)?;
        let mut parts: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
        let mut curves: Vec<Vec<[u64; 2]>> = Vec::new();
        for _ in 0..self.env.samples {
            let start = Instant::now();
            let channel = compat::dial(&self.env.endpoint, &self.env.settings)?;
            let dialed = start.elapsed();
            let link = compat::prove(channel, &self.env.proof, &self.env.settings)?;
            let proven = start.elapsed();
            let mut header = None;
            let mut first = None;
            let mut received = 0_u64;
            let mut curve: Vec<[u64; 2]> = vec![[0, 0]];
            let mut next_mark = 100_u64;
            compat::read_observed(
                &link,
                &file,
                || header = Some(start.elapsed()),
                |count| {
                    let now = start.elapsed();
                    first.get_or_insert(now);
                    received += count as u64;
                    let at = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
                    if at >= next_mark {
                        curve.push([at, received]);
                        next_mark = at - at % 100 + 100;
                    }
                },
            )?;
            let last = start.elapsed();
            curve.push([
                u64::try_from(last.as_millis()).unwrap_or(u64::MAX),
                received,
            ]);
            let header = header.unwrap_or(last);
            let first = first.unwrap_or(last);
            parts.entry("dial").or_default().push(ms(dialed));
            parts.entry("proof").or_default().push(ms(proven - dialed));
            parts.entry("open").or_default().push(ms(header - proven));
            parts.entry("first").or_default().push(ms(first - header));
            parts.entry("body").or_default().push(ms(last - first));
            parts.entry("total").or_default().push(ms(last));
            curves.push(curve);
        }
        for (part, values) in parts {
            let moved = (part == "body" || part == "total").then_some(bytes);
            self.record_values(&format!("{name}_{part}"), &values, moved);
        }
        self.out
            .curves
            .insert(format!("{name}_{}", self.env.suffix), curves);
        Ok(())
    }
}

/// `bohio remote ENDPOINT ... action` as a child, its output thrown away; ends when it does.
fn remote_child(env: &Env, action: &[&str]) -> Result<(), String> {
    let mut command = Command::new(&env.bohio);
    command
        .arg("remote")
        .arg(&env.endpoint_text)
        .arg("--config")
        .arg(&env.client_config);
    if let Some(server) = &env.server {
        command.args(["--server", server]);
    }
    let status = command
        .args(action)
        .env_remove("BOHIO_CONFIG")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("{}: {error}", env.bohio.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("bohio remote {action:?} ended with {status}"))
    }
}
