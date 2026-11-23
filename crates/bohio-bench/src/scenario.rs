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
    /// A read of the `text` or `random` corpus file, compressed as the run says or as the name's algorithm says
    /// (`read_text_1MiB_zstd3`), on a ready link.
    #[cfg(feature = "compress")]
    ReadCorpus {
        corpus: String,
        size: String,
        algorithm: Option<String>,
    },
    /// A write of that much of a corpus kind, compressed likewise.
    #[cfg(feature = "compress")]
    WriteCorpus {
        corpus: String,
        size: String,
        algorithm: Option<String>,
    },
    /// A program that writes that much text to standard output (and the same to standard error), the output
    /// compressed likewise.
    #[cfg(feature = "compress")]
    ExecText {
        size: String,
        algorithm: Option<String>,
    },
    /// A file of this size changed as `edit` says, moved to or from the host with a delta (`delta_...`) or whole
    /// (`full_...`), on a ready link.
    #[cfg(feature = "delta")]
    Sync {
        delta: bool,
        push: bool,
        size: String,
        edit: Edit,
    },
    /// One walk stream over a tree of this many small files, made first and left alone (`walk_10k`).
    #[cfg(feature = "walk")]
    Walk(u64),
    /// This many files of this size read (`read`) or written as one bundle on one stream (`bundle_read_1000x4KiB`).
    #[cfg(feature = "walk")]
    Bundle {
        read: bool,
        count: u64,
        size: String,
    },
    /// The same files, one stream for each (`file_read_1000x4KiB`): the control a bundle is measured against. Needs the
    /// client link's `max_opens_per_minute` raised past the count (`--set link.max_opens_per_minute=...`).
    #[cfg(feature = "walk")]
    PerFile {
        read: bool,
        count: u64,
        size: String,
    },
    /// A second `sync_tree` of a local tree of this many small files that is already on the host (`sync_tree_unchanged_10k`):
    /// it moves no file.
    #[cfg(feature = "walk")]
    SyncUnchanged(u64),
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

/// What changed between the old file and the new one in a delta scenario.
#[cfg(feature = "delta")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// This many bytes in the middle.
    Bytes(u64),
    /// This many percent of the file as one run.
    Run(u64),
    /// This many percent of the file as runs of 4 KiB spread over it.
    Scatter(u64),
}

#[cfg(feature = "delta")]
fn edit_of(text: &str, name: &str) -> Result<Edit, String> {
    let bad = || format!("`{text}` in `{name}` is not an edit like 1byte, 1pct or 1pctscatter");
    if let Some(count) = text.strip_suffix("byte") {
        return count.parse().map(Edit::Bytes).map_err(|_| bad());
    }
    if let Some(count) = text.strip_suffix("pctscatter") {
        return count.parse().map(Edit::Scatter).map_err(|_| bad());
    }
    text.strip_suffix("pct")
        .and_then(|count| count.parse().ok())
        .map(Edit::Run)
        .ok_or_else(bad)
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
        #[cfg(feature = "walk")]
        if let Some((read, rest)) = direction_of(name, "bundle_") {
            let (count, size) = files_of(rest, name)?;
            return Ok(Self::Bundle { read, count, size });
        }
        #[cfg(feature = "walk")]
        if let Some((read, rest)) = direction_of(name, "file_") {
            let (count, size) = files_of(rest, name)?;
            return Ok(Self::PerFile { read, count, size });
        }
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
            #[cfg(feature = "compress")]
            ["read", corpus @ ("text" | "random"), s, rest @ ..] if rest.len() <= 1 => {
                Self::ReadCorpus {
                    corpus: (*corpus).to_owned(),
                    size: size(s)?,
                    algorithm: rest.first().map(|text| (*text).to_owned()),
                }
            }
            #[cfg(feature = "compress")]
            ["write", corpus @ ("text" | "random"), s, rest @ ..] if rest.len() <= 1 => {
                Self::WriteCorpus {
                    corpus: (*corpus).to_owned(),
                    size: size(s)?,
                    algorithm: rest.first().map(|text| (*text).to_owned()),
                }
            }
            #[cfg(feature = "compress")]
            ["exec", "text", s, rest @ ..] if rest.len() <= 1 => Self::ExecText {
                size: size(s)?,
                algorithm: rest.first().map(|text| (*text).to_owned()),
            },
            #[cfg(feature = "delta")]
            [
                mode @ ("delta" | "full"),
                direction @ ("push" | "pull"),
                s,
                edit,
            ] => Self::Sync {
                delta: *mode == "delta",
                push: *direction == "push",
                size: size(s)?,
                edit: edit_of(edit, name)?,
            },
            #[cfg(feature = "walk")]
            ["walk", n] => Self::Walk(thousands(n, name)?),
            #[cfg(feature = "walk")]
            ["sync", "tree", "unchanged", n] => Self::SyncUnchanged(thousands(n, name)?),
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

/// A count like `10k` or `500`.
#[cfg(feature = "walk")]
fn thousands(text: &str, name: &str) -> Result<u64, String> {
    let (digits, factor) = text
        .strip_suffix('k')
        .map_or((text, 1), |digits| (digits, 1000));
    digits
        .parse::<u64>()
        .ok()
        .and_then(|count| count.checked_mul(factor))
        .ok_or_else(|| format!("`{text}` in `{name}` is not a count like 500 or 10k"))
}

/// `prefix` then `read_` or `write_`: which, and what follows.
#[cfg(feature = "walk")]
fn direction_of<'a>(name: &'a str, prefix: &str) -> Option<(bool, &'a str)> {
    let rest = name.strip_prefix(prefix)?;
    rest.strip_prefix("read_")
        .map(|rest| (true, rest))
        .or_else(|| rest.strip_prefix("write_").map(|rest| (false, rest)))
}

/// `1000x4KiB`: a count of files and the size of each.
#[cfg(feature = "walk")]
fn files_of(text: &str, name: &str) -> Result<(u64, String), String> {
    let (count, size) = text
        .split_once('x')
        .ok_or_else(|| format!("`{text}` in `{name}` is not files like 1000x4KiB"))?;
    parse_size(size)?;
    Ok((thousands(count, name)?, size.to_owned()))
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
    /// What the compression scenarios ask for unless a name says: `ALG[:LEVEL]`, none when absent (`--set bench.compress=`).
    #[cfg_attr(
        not(feature = "compress"),
        allow(dead_code, reason = "read only by the compression scenarios")
    )]
    pub compress: Option<String>,
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

    /// The compression a name asks for (`zstd3`, `lz4`, `deflate6`, `none`), else the run's `bench.compress`.
    #[cfg(feature = "compress")]
    fn compression_of(
        &self,
        named: Option<&str>,
    ) -> Result<Option<bohio_proto::compress::Compression>, String> {
        match named {
            Some(text) => {
                // `zstd3` is zstd at level 3; `lz4` and `none` carry no level.
                let spelled = ["zstd", "deflate", "lz4", "none"]
                    .iter()
                    .find_map(|name| text.strip_prefix(name).map(|level| (*name, level)))
                    .map_or_else(
                        || text.to_owned(),
                        |(name, level)| {
                            if level.is_empty() {
                                name.to_owned()
                            } else {
                                format!("{name}:{level}")
                            }
                        },
                    );
                compat::compression(&spelled)
            }
            None => self
                .env
                .compress
                .as_deref()
                .map_or(Ok(None), compat::compression),
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
            #[cfg(feature = "compress")]
            Kind::ReadCorpus {
                corpus,
                size,
                algorithm,
            } => {
                let wanted = self.compression_of(algorithm.as_deref())?;
                let file = self.path(&format!("{corpus}-{size}"));
                let link = warm_link(&self.env, &mut self.warm)?;
                // One read first: the link is past slow start, as `read_<size>_warm` has it.
                compat::read_compressed(link, &file, None)?;
                let mut wire = Vec::new();
                let taken = timed(&mut self.env.cpu, samples, || {
                    let (_, moved) = compat::read_compressed(link, &file, wanted.as_ref())?;
                    wire.push(moved as f64);
                    Ok(())
                })?;
                self.record(name, &taken, Some(parse_size(&size)?));
                self.record_values(&format!("{name}_wire_bytes"), &wire, None);
            }
            #[cfg(feature = "compress")]
            Kind::WriteCorpus {
                corpus,
                size,
                algorithm,
            } => {
                let wanted = self.compression_of(algorithm.as_deref())?;
                let bytes = parse_size(&size)?;
                let target = self.path(&format!("written/write-{corpus}-{size}.bin"));
                let link = warm_link(&self.env, &mut self.warm)?;
                let fs = compat::fs(link)?;
                let taken = timed(&mut self.env.cpu, samples, || {
                    compat::write_compressed(link, &target, &corpus, bytes, wanted.as_ref())
                })?;
                compat::remove(&fs, &target);
                self.record(name, &taken, Some(bytes));
            }
            #[cfg(feature = "compress")]
            Kind::ExecText { size, algorithm } => {
                let wanted = self.compression_of(algorithm.as_deref())?;
                let program = self
                    .env
                    .program
                    .clone()
                    .ok_or("no --program: the exec scenarios are skipped")?;
                let bytes = parse_size(&size)?;
                let argv = vec![program, "text".to_owned(), bytes.to_string()];
                let link = warm_link(&self.env, &mut self.warm)?;
                let taken = timed(&mut self.env.cpu, samples, || {
                    compat::exec_compressed(link, &argv, wanted.as_ref()).map(drop)
                })?;
                self.record(name, &taken, Some(bytes));
            }
            #[cfg(feature = "delta")]
            Kind::Sync {
                delta,
                push,
                size,
                edit,
            } => self.sync(name, delta, push, &size, edit)?,
            #[cfg(feature = "walk")]
            Kind::Walk(count) => {
                let dir = self.path(&format!("written/walk-{count}"));
                let link = warm_link(&self.env, &mut self.warm)?;
                // The tree is made once, in bundles, and not timed.
                let present = compat::walk_tree(link, &dir).unwrap_or(0);
                if present <= count {
                    for start in (0..count).step_by(2000) {
                        let part = (count - start).min(2000);
                        compat::bundle_write(link, &format!("{dir}/part{start:07}"), part, 16)?;
                    }
                }
                let taken = timed(&mut self.env.cpu, samples, || {
                    compat::walk_tree(link, &dir).map(drop)
                })?;
                self.record(name, &taken, None);
            }
            #[cfg(feature = "walk")]
            Kind::Bundle { read, count, size } => {
                let bytes = parse_size(&size)?;
                let dir = self.path(&format!("written/files-{count}x{size}"));
                let link = warm_link(&self.env, &mut self.warm)?;
                if read {
                    compat::bundle_write(link, &dir, count, bytes)?;
                    let taken = timed(&mut self.env.cpu, samples, || {
                        compat::bundle_read(link, &dir, count).map(drop)
                    })?;
                    self.record(name, &taken, Some(bytes * count));
                } else {
                    let taken = timed(&mut self.env.cpu, samples, || {
                        compat::bundle_write(link, &dir, count, bytes).map(drop)
                    })?;
                    self.record(name, &taken, Some(bytes * count));
                }
            }
            #[cfg(feature = "walk")]
            Kind::PerFile { read, count, size } => {
                let bytes = parse_size(&size)?;
                let dir = self.path(&format!("written/files-{count}x{size}"));
                let link = warm_link(&self.env, &mut self.warm)?;
                if read {
                    compat::bundle_write(link, &dir, count, bytes)?;
                    let taken = timed(&mut self.env.cpu, samples, || {
                        compat::file_read(link, &dir, count).map(drop)
                    })?;
                    self.record(name, &taken, Some(bytes * count));
                } else {
                    let taken = timed(&mut self.env.cpu, samples, || {
                        compat::file_write(link, &dir, count, bytes)
                    })?;
                    self.record(name, &taken, Some(bytes * count));
                }
            }
            #[cfg(feature = "walk")]
            Kind::SyncUnchanged(count) => {
                let scratch = Scratch::new(name)?;
                for index in 0..count {
                    let path = scratch
                        .dir
                        .join(format!("d{:03}/f{index:05}.bin", index % 100));
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                    }
                    std::fs::write(&path, index.to_le_bytes())
                        .map_err(|error| error.to_string())?;
                }
                let remote = self.path(&format!("written/sync-tree-{count}"));
                let link = warm_link(&self.env, &mut self.warm)?;
                // The first sync puts the tree on the host; only the later ones are timed.
                compat::sync_tree_push(link, &scratch.dir, &remote)?;
                let mut wires = Vec::new();
                let taken = timed(&mut self.env.cpu, samples, || {
                    let (moved, wire) = compat::sync_tree_push(link, &scratch.dir, &remote)?;
                    if moved != 0 {
                        return Err(format!("{name}: {moved} files moved though none changed"));
                    }
                    wires.push(wire as f64);
                    Ok(())
                })?;
                self.record(name, &taken, None);
                self.record_values(&format!("{name}_wire_bytes"), &wires, None);
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

    /// A delta scenario: the host holds one version of a file and this side the other; each sample puts the files
    /// back (untimed) and times one transfer. The metrics are the time, and the bytes of frames that travelled.
    #[cfg(feature = "delta")]
    fn sync(
        &mut self,
        name: &str,
        delta: bool,
        push: bool,
        size: &str,
        edit: Edit,
    ) -> Result<(), String> {
        let bytes = parse_size(size)?;
        let scratch = Scratch::new(name)?;
        let (old, new, work) = (
            scratch.dir.join("old.bin"),
            scratch.dir.join("new.bin"),
            scratch.dir.join("work.bin"),
        );
        random_file(&old, bytes, 21)?;
        std::fs::copy(&old, &new).map_err(|error| error.to_string())?;
        edit_file(&new, bytes, edit)?;
        let remote = self.path(&format!("written/sync-{size}-{}.bin", std::process::id()));
        let link = warm_link(&self.env, &mut self.warm)?;
        let fs = compat::fs(link)?;
        let mut taken = Samples {
            wall: Vec::new(),
            cpu: Vec::new(),
        };
        let mut wires = Vec::new();
        for _ in 0..self.env.samples {
            // The host and this side start from the two versions; neither step is timed.
            if push {
                compat::sync_put(link, &old, &remote, false)?;
            } else {
                compat::sync_put(link, &new, &remote, false)?;
                std::fs::copy(&old, &work).map_err(|error| error.to_string())?;
            }
            let before = self.env.cpu.as_mut().map(Cpu::used_ms);
            let start = Instant::now();
            let wire = if push {
                compat::sync_put(link, &new, &remote, delta)?
            } else {
                compat::sync_get(link, &remote, &work, delta)?
            };
            taken.wall.push(ms(start.elapsed()));
            if let (Some(cpu), Some(before)) = (self.env.cpu.as_mut(), before) {
                taken.cpu.push(cpu.used_ms().saturating_sub(before) as f64);
            }
            wires.push(wire as f64);
            let landed = if push { &new } else { &work };
            if !compat::same_bytes(link, &remote, landed)? {
                return Err(format!(
                    "{name}: the file that landed differs from the new one"
                ));
            }
        }
        compat::remove(&fs, &remote);
        self.record(name, &taken, Some(bytes));
        self.record_values(&format!("{name}_wire_bytes"), &wires, None);
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

/// A scratch directory of a delta or tree scenario, removed when it goes: under the system's temporary directory (`TMPDIR`).
#[cfg(any(feature = "delta", feature = "walk"))]
struct Scratch {
    dir: PathBuf,
}

#[cfg(any(feature = "delta", feature = "walk"))]
impl Scratch {
    fn new(name: &str) -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!("bohio-bench-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        Ok(Self { dir })
    }
}

#[cfg(any(feature = "delta", feature = "walk"))]
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Writes `bytes` bytes of a fixed pseudo-random sequence to `path`.
#[cfg(feature = "delta")]
fn random_file(path: &std::path::Path, bytes: u64, seed: u64) -> Result<(), String> {
    use std::io::Write;
    let mut out =
        std::io::BufWriter::new(std::fs::File::create(path).map_err(|error| error.to_string())?);
    let mut random = Xorshift::new(seed);
    let mut block = vec![0_u8; 1 << 20];
    let mut left = bytes;
    while left > 0 {
        random.fill(&mut block);
        let take = usize::try_from(left.min(block.len() as u64)).unwrap_or(block.len());
        out.write_all(&block[..take])
            .map_err(|error| error.to_string())?;
        left -= take as u64;
    }
    out.flush().map_err(|error| error.to_string())
}

/// Overwrites parts of `path` with other pseudo-random bytes, as `edit` says.
#[cfg(feature = "delta")]
fn edit_file(path: &std::path::Path, size: u64, edit: Edit) -> Result<(), String> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    let mut random = Xorshift::new(99);
    let mut overwrite = |at: u64, length: u64| -> Result<(), String> {
        let mut block = vec![0_u8; usize::try_from(length).map_err(|error| error.to_string())?];
        random.fill(&mut block);
        file.seek(SeekFrom::Start(at))
            .map_err(|error| error.to_string())?;
        file.write_all(&block).map_err(|error| error.to_string())
    };
    match edit {
        Edit::Bytes(count) => overwrite(size / 3, count.min(size))?,
        Edit::Run(percent) => overwrite(size / 3, (size / 100 * percent).min(size - size / 3))?,
        Edit::Scatter(percent) => {
            const RUN: u64 = 4096;
            let runs = (size / 100 * percent / RUN).max(1);
            let stride = size / runs;
            for index in 0..runs {
                let at = index * stride + stride.saturating_sub(RUN) / 2;
                overwrite(at, RUN.min(size - at))?;
            }
        }
    }
    Ok(())
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
